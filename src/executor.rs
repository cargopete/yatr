//! Task execution engine
//!
//! Handles running commands, managing parallel execution, and coordinating
//! with the cache and scripting systems.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::cache::Cache;
use crate::config::Config;
use crate::error::{Result, YatrError};
use crate::graph::{ExecutionPlan, TaskGraph, TaskNode};
use crate::output::{errln, out, outln};
use crate::script::ScriptEngine;

/// Result of executing a single task
#[derive(Debug)]
pub struct TaskResult {
    pub name: String,
    pub success: bool,
    pub duration: Duration,
    /// Offset from the start of the run until this task began (for profiling)
    pub start_offset: Duration,
    pub cached: bool,
    pub output: Option<String>,
    pub error: Option<String>,
    /// Exit code of the failing command, when the task failed
    pub exit_code: Option<i32>,
}

/// Everything a task printed, stdout and stderr together.
#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<String>>);

impl Capture {
    fn push(&self, text: &str) {
        if let Ok(mut buf) = self.0.lock() {
            buf.push_str(text);
        }
    }

    fn take(&self) -> String {
        self.0
            .lock()
            .map(|mut buf| std::mem::take(&mut *buf))
            .unwrap_or_default()
    }
}

/// Echoes a running task's lines to the terminal, prefixed with its name.
#[derive(Clone)]
struct Echo {
    prefix: String,
    progress: MultiProgress,
}

impl Echo {
    fn line(&self, line: &str, to_stderr: bool) {
        let prefix = if line.is_empty() {
            self.prefix.trim_end()
        } else {
            &self.prefix
        };
        self.progress.suspend(|| {
            if to_stderr {
                errln!("{prefix}{line}");
            } else {
                outln!("{prefix}{line}");
            }
        });
    }

    /// Print output that was not streamed (a cache hit, a script, a plugin).
    fn replay(&self, output: &str) {
        let body = output.trim_end();
        let first = body.lines().position(|l| !l.trim().is_empty());
        for line in body.lines().skip(first.unwrap_or(usize::MAX)) {
            self.line(line, false);
        }
    }
}

/// Where a command's output goes: always into a capture, and live to the
/// terminal unless the run is emitting JSON.
#[derive(Clone)]
struct Sink {
    capture: Capture,
    echo: Option<Echo>,
}

impl Sink {
    fn with_capture(&self, capture: Capture) -> Self {
        Self {
            capture,
            echo: self.echo.clone(),
        }
    }

    /// Record output produced in one piece rather than streamed.
    fn push_whole(&self, output: &str) {
        self.capture.push(output);
        if let Some(echo) = &self.echo {
            echo.replay(output);
        }
    }
}

/// Copy one of a child's output streams into the sink, line by line.
async fn pump(stream: impl tokio::io::AsyncRead + Unpin, sink: Sink, to_stderr: bool) {
    use tokio::io::AsyncBufReadExt;

    let mut reader = tokio::io::BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        sink.capture.push(&text);
        if let Some(echo) = &sink.echo {
            echo.line(text.trim_end_matches(['\n', '\r']), to_stderr);
        }
    }
}

/// Executor configuration
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Number of parallel tasks (0 = number of CPUs)
    pub parallelism: usize,
    /// Dry run mode (don't execute, just show plan)
    pub dry_run: bool,
    /// Force run even if cached
    pub force: bool,
    /// Working directory
    pub cwd: std::path::PathBuf,
    /// Use shell for commands
    pub shell: bool,
    /// Verbose output
    pub verbose: bool,
    /// Suppress human output; caller emits machine-readable JSON instead
    pub json: bool,
    /// Warn when a task writes files outside its declared `outputs`
    pub trace_io: bool,
    /// Reference instant for the whole run, used to compute task start offsets
    pub run_start: Instant,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            parallelism: 0,
            dry_run: false,
            force: false,
            cwd: std::env::current_dir().unwrap_or_default(),
            shell: false,
            verbose: false,
            json: false,
            trace_io: false,
            run_start: Instant::now(),
        }
    }
}

/// Task executor
pub struct Executor {
    config: Arc<Config>,
    exec_config: ExecutorConfig,
    cache: Option<Cache>,
    script_engine: ScriptEngine,
}

/// Cache fingerprints of finished tasks, keyed by task name.
type Fingerprints = std::sync::Mutex<HashMap<String, String>>;

impl Executor {
    /// Create a new executor
    #[must_use]
    pub fn new(config: Config, exec_config: ExecutorConfig, cache: Option<Cache>) -> Self {
        Self {
            config: Arc::new(config),
            exec_config,
            cache,
            script_engine: ScriptEngine::new(),
        }
    }

    /// Execute tasks according to the execution plan
    pub async fn execute(&self, graph: &TaskGraph, task_name: &str) -> Result<Vec<TaskResult>> {
        let mut results = Vec::new();
        self.execute_into(graph, task_name, &mut results)
            .await
            .map(|()| results)
    }

    /// As [`Executor::execute`], but results of tasks that finished before a
    /// failure are kept in `all_results` rather than lost with the error.
    pub async fn execute_into(
        &self,
        graph: &TaskGraph,
        task_name: &str,
        all_results: &mut Vec<TaskResult>,
    ) -> Result<()> {
        let tasks = graph.execution_order(task_name)?;

        if self.exec_config.dry_run {
            if !self.exec_config.json {
                let plan = ExecutionPlan::from_tasks(tasks, graph);
                self.print_dry_run(&plan);
            }
            return Ok(());
        }

        let parallelism = if self.exec_config.parallelism == 0 {
            std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
        } else {
            self.exec_config.parallelism
        };

        let semaphore = Arc::new(Semaphore::new(parallelism));
        let multi_progress = MultiProgress::new();
        let name_width = tasks.iter().map(|t| t.name.len()).max().unwrap_or(0);

        // Ready-queue scheduling: a task starts the moment all of its
        // dependencies have completed, rather than waiting for its whole
        // dependency "level" — better wall-clock when task durations are uneven.
        let (node_map, mut in_degree, dependents) = Self::build_dag(&tasks, graph);
        // Cache fingerprints of finished tasks, folded into their dependents' keys.
        let fingerprints: Arc<Fingerprints> = Arc::default();

        // Build the future that runs one task, gated by the concurrency semaphore.
        let spawn_one = |name: &str| {
            let task = node_map[name].clone();
            let config = Arc::clone(&self.config);
            let sem = Arc::clone(&semaphore);
            let exec_config = self.exec_config.clone();
            let cache = self.cache.clone();
            let mp = multi_progress.clone();
            let fingerprints = Arc::clone(&fingerprints);
            let dep_keys = Self::dep_fingerprints(graph, name, &fingerprints);
            async move {
                let _permit = sem.acquire().await.map_err(|e| {
                    YatrError::Io(std::io::Error::other(format!(
                        "Semaphore acquire failed: {e}"
                    )))
                })?;

                // Progress spinner is human-only; suppress it in JSON mode.
                let pb = (!exec_config.json).then(|| {
                    let pb = mp.add(ProgressBar::new_spinner());
                    let style = ProgressStyle::default_spinner()
                        .template("{spinner:.cyan} {msg}")
                        .unwrap_or_else(|_| ProgressStyle::default_spinner());
                    pb.set_style(style);
                    pb.set_message(format!("Running {}", task.name));
                    pb.enable_steady_tick(Duration::from_millis(100));
                    pb
                });

                let echo = (!exec_config.json).then(|| Echo {
                    prefix: format!("{} ", style(format!("{:<name_width$} │", task.name)).dim()),
                    progress: mp.clone(),
                });
                let result = Self::execute_single_task(
                    &task,
                    &config,
                    &exec_config,
                    cache.as_ref(),
                    &dep_keys,
                    &fingerprints,
                    echo,
                )
                .await;

                if let Some(pb) = pb {
                    pb.finish_and_clear();
                }
                result
            }
        };

        let mut running = JoinSet::new();
        for name in node_map.keys() {
            if in_degree[name] == 0 {
                running.spawn(spawn_one(name));
            }
        }

        let first = all_results.len();
        while let Some(joined) = running.join_next().await {
            let result =
                joined.map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))??;

            let success = result.success;
            let finished = result.name.clone();
            let allow_failure = graph
                .get_task(&finished)
                .is_some_and(|t| t.config.allow_failure);

            if !self.exec_config.json {
                Self::print_task_result(&result);
            }
            all_results.push(result);

            if !success && !allow_failure {
                let code = all_results.last().and_then(|r| r.exit_code).unwrap_or(1);
                return Err(YatrError::TaskFailed {
                    task: finished,
                    code,
                    stderr: None,
                });
            }

            // Unblock dependents whose final dependency just completed.
            if let Some(deps) = dependents.get(&finished) {
                for d in deps {
                    if let Some(remaining) = in_degree.get_mut(d) {
                        *remaining -= 1;
                        if *remaining == 0 {
                            running.spawn(spawn_one(d));
                        }
                    }
                }
            }
        }

        if !self.exec_config.json {
            self.print_summary(&all_results[first..]);
        }
        Ok(())
    }

    /// Build the dependency bookkeeping for the ready-queue scheduler:
    /// name → task node, name → outstanding dependency count, and
    /// name → tasks that depend on it (all scoped to the tasks being run).
    #[allow(clippy::type_complexity)]
    fn build_dag(
        tasks: &[&TaskNode],
        graph: &TaskGraph,
    ) -> (
        HashMap<String, TaskNode>,
        HashMap<String, usize>,
        HashMap<String, Vec<String>>,
    ) {
        let in_set: HashSet<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        let node_map: HashMap<String, TaskNode> = tasks
            .iter()
            .map(|t| (t.name.clone(), (*t).clone()))
            .collect();
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for task in tasks {
            let deps: Vec<String> = graph
                .dependencies(&task.name)
                .unwrap_or_default()
                .into_iter()
                .filter(|d| in_set.contains(d))
                .map(String::from)
                .collect();
            in_degree.insert(task.name.clone(), deps.len());
            for d in deps {
                dependents.entry(d).or_default().push(task.name.clone());
            }
        }
        (node_map, in_degree, dependents)
    }

    fn dep_fingerprints(graph: &TaskGraph, name: &str, fingerprints: &Fingerprints) -> Vec<String> {
        let deps = graph.dependencies(name).unwrap_or_default();
        fingerprints.lock().map_or_else(
            |_| Vec::new(),
            |f| {
                deps.iter()
                    .map(|d| f.get(*d).cloned().unwrap_or_else(|| (*d).to_string()))
                    .collect()
            },
        )
    }

    fn record_fingerprint(
        fingerprints: &Fingerprints,
        task: &TaskNode,
        key: Option<&str>,
        cwd: &Path,
    ) {
        if let (Some(key), Ok(mut f)) = (key, fingerprints.lock()) {
            f.insert(
                task.name.clone(),
                Cache::fingerprint(key, &task.config, cwd),
            );
        }
    }

    /// Execute a single task
    async fn execute_single_task(
        task: &TaskNode,
        config: &Config,
        exec_config: &ExecutorConfig,
        cache: Option<&Cache>,
        dep_keys: &[String],
        fingerprints: &Fingerprints,
        echo: Option<Echo>,
    ) -> Result<TaskResult> {
        let start = Instant::now();
        let start_offset = exec_config.run_start.elapsed();
        let env = config.task_env(&task.config);

        // Determine working directory (needed for cache key + output restore)
        let cwd = task
            .config
            .cwd
            .clone()
            .unwrap_or_else(|| exec_config.cwd.clone());

        let key = cache
            .map(|_| Cache::compute_key(&task.name, &task.config, config, dep_keys, &cwd))
            .transpose()?;
        let cacheable = Cache::caches(&task.config);
        let record_fingerprint =
            || Self::record_fingerprint(fingerprints, task, key.as_deref(), &cwd);

        // Check cache
        if !exec_config.force {
            if let (Some(cache), Some(key)) = (cache, &key) {
                if cacheable {
                    if let Some(cached) = cache.get(&task.name, key, &cwd).await? {
                        record_fingerprint();
                        if let Some(echo) = &echo {
                            echo.replay(&cached);
                        }
                        return Ok(TaskResult {
                            name: task.name.clone(),
                            success: true,
                            duration: start.elapsed(),
                            start_offset,
                            cached: true,
                            output: Some(cached),
                            error: None,
                            exit_code: None,
                        });
                    }
                }
            }
        }

        // Use task-level shell setting if specified, otherwise use exec_config
        let mut task_exec_config = exec_config.clone();
        if task.config.shell.unwrap_or(false) {
            task_exec_config.shell = true;
        }

        // IO tracing: snapshot the tree before a (non-foreground) run.
        let trace_before =
            (exec_config.trace_io && !task.config.foreground).then(|| crate::trace::snapshot(&cwd));

        let sink = Sink {
            capture: Capture::default(),
            echo,
        };
        let result = Self::run_task_body(task, &env, &cwd, &task_exec_config, &sink).await;

        let duration = start.elapsed();
        record_fingerprint();

        // IO tracing: report files written outside the declared `outputs`.
        if let Some(before) = trace_before {
            Self::report_io_trace(&task.name, &cwd, &task.config.outputs, &before);
        }

        let output = sink.capture.take();
        match result {
            Ok(()) => {
                // Store in cache (skip foreground tasks: their output isn't captured)
                if let (Some(cache), Some(key)) = (cache, &key) {
                    if cacheable && !task.config.foreground {
                        let _ = cache
                            .put(&task.name, key, &task.config, &cwd, &output, duration)
                            .await;
                    }
                }

                Ok(TaskResult {
                    name: task.name.clone(),
                    success: true,
                    duration,
                    start_offset,
                    cached: false,
                    output: Some(output),
                    error: None,
                    exit_code: None,
                })
            }
            Err(e) => Ok(TaskResult {
                name: task.name.clone(),
                success: false,
                duration,
                start_offset,
                cached: false,
                output: Some(output),
                exit_code: Some(i32::from(e.exit_code())),
                error: Some(e.to_string()),
            }),
        }
    }

    /// Run whichever kind of body a task has, capturing what it prints.
    async fn run_task_body(
        task: &TaskNode,
        env: &HashMap<String, String>,
        cwd: &Path,
        exec_config: &ExecutorConfig,
        sink: &Sink,
    ) -> Result<()> {
        if task.config.foreground {
            // Execute in foreground with inherited stdio (for long-running processes)
            Self::execute_foreground(&task.name, &task.config.run, env, cwd, exec_config)
                .await
                .map(|out| sink.capture.push(&out))
        } else if let Some(wasm) = &task.config.wasm {
            // Execute a sandboxed WASM plugin (local path or downloaded URL)
            Self::execute_wasm(&task.name, wasm, cwd, env)
                .await
                .map(|out| sink.push_whole(&out))
        } else if let Some(script) = &task.config.script {
            // Execute Rhai script
            Self::execute_script(&task.name, script, env, cwd).map(|out| sink.push_whole(&out))
        } else if task.config.parallel {
            // Execute commands in parallel
            Self::execute_commands_parallel(
                &task.name,
                &task.config.run,
                env,
                cwd,
                exec_config,
                sink,
            )
            .await
        } else {
            // Execute commands sequentially
            Self::execute_commands_sequential(
                &task.name,
                &task.config.run,
                env,
                cwd,
                exec_config,
                sink,
            )
            .await
        }
    }

    /// Warn about files a task wrote outside its declared `outputs` (IO trace).
    fn report_io_trace(
        task_name: &str,
        cwd: &Path,
        outputs: &[String],
        before: &crate::trace::Snapshot,
    ) {
        let after = crate::trace::snapshot(cwd);
        let undeclared = crate::trace::undeclared_writes(before, &after, outputs);
        if !undeclared.is_empty() {
            errln!(
                "{} task '{task_name}' wrote files not declared as `outputs`: {}",
                style("trace:").yellow().bold(),
                undeclared.join(", ")
            );
        }
    }

    /// Execute a Rhai script
    fn execute_script(
        task_name: &str,
        script: &str,
        env: &HashMap<String, String>,
        cwd: &Path,
    ) -> Result<String> {
        let engine = ScriptEngine::new();
        engine
            .execute(script, env, cwd)
            .map_err(|e| YatrError::ScriptFailed {
                task: task_name.to_string(),
                source: e,
            })
    }

    /// Execute a sandboxed WASM plugin. The reference is a local path (resolved
    /// relative to the task's working directory) or an `http(s)://` URL that is
    /// downloaded and cached. The task name and environment are passed to the
    /// plugin as JSON input.
    async fn execute_wasm(
        task_name: &str,
        wasm: &Path,
        cwd: &Path,
        env: &HashMap<String, String>,
    ) -> Result<String> {
        let path = crate::wasm::resolve_plugin(&wasm.to_string_lossy(), cwd, task_name).await?;
        let input = serde_json::json!({ "task": task_name, "env": env });
        let input_bytes = serde_json::to_vec(&input).unwrap_or_default();
        crate::wasm::run_plugin(&path, task_name, &input_bytes)
    }

    /// Execute commands sequentially
    async fn execute_commands_sequential(
        _task_name: &str,
        commands: &[String],
        env: &HashMap<String, String>,
        cwd: &Path,
        exec_config: &ExecutorConfig,
        sink: &Sink,
    ) -> Result<()> {
        for cmd in commands {
            Self::execute_command(cmd, env, cwd, exec_config, sink).await?;
        }
        Ok(())
    }

    /// Execute commands in foreground with inherited stdio
    async fn execute_foreground(
        task_name: &str,
        commands: &[String],
        env: &HashMap<String, String>,
        cwd: &Path,
        exec_config: &ExecutorConfig,
    ) -> Result<String> {
        // Foreground tasks run with inherited stdio and block until completion
        // Only the first command is executed (foreground doesn't make sense for multiple commands)
        let cmd = commands.first().ok_or_else(|| YatrError::InvalidTask {
            task: task_name.to_string(),
            reason: "Foreground task must have at least one command".to_string(),
        })?;

        let parts = Self::parse_command(cmd, exec_config.shell);

        let mut command = if exec_config.shell {
            let shell = if cfg!(windows) { "cmd" } else { "sh" };
            let flag = if cfg!(windows) { "/C" } else { "-c" };
            let mut c = Command::new(shell);
            c.arg(flag).arg(cmd);
            c
        } else {
            let mut c = Command::new(&parts[0]);
            if parts.len() > 1 {
                c.args(&parts[1..]);
            }
            c
        };

        command
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let status = command.status().await?;

        if !status.success() {
            return Err(YatrError::CommandFailed {
                command: cmd.clone(),
                code: exit_code(status),
            });
        }

        Ok(String::from("(foreground task completed)"))
    }

    /// Execute commands in parallel
    async fn execute_commands_parallel(
        _task_name: &str,
        commands: &[String],
        env: &HashMap<String, String>,
        cwd: &Path,
        exec_config: &ExecutorConfig,
        sink: &Sink,
    ) -> Result<()> {
        let mut handles = Vec::new();

        for cmd in commands {
            let cmd = cmd.clone();
            let env = env.clone();
            let cwd = cwd.to_path_buf();
            let exec_config = exec_config.clone();
            let own = Capture::default();
            let theirs = sink.with_capture(own.clone());

            handles.push((
                own,
                tokio::spawn(async move {
                    Self::execute_command(&cmd, &env, &cwd, &exec_config, &theirs).await
                }),
            ));
        }

        let mut first_err = None;
        for (own, handle) in handles {
            let result = handle
                .await
                .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))
                .and_then(|r| r);
            sink.capture.push(&own.take());
            if let Err(e) = result {
                first_err.get_or_insert(e);
            }
        }

        first_err.map_or(Ok(()), Err)
    }

    /// Execute a single command
    async fn execute_command(
        cmd: &str,
        env: &HashMap<String, String>,
        cwd: &Path,
        exec_config: &ExecutorConfig,
        sink: &Sink,
    ) -> Result<()> {
        let parts = Self::parse_command(cmd, exec_config.shell);

        let mut command = if exec_config.shell {
            let shell = if cfg!(windows) { "cmd" } else { "sh" };
            let flag = if cfg!(windows) { "/C" } else { "-c" };
            let mut c = Command::new(shell);
            c.arg(flag).arg(cmd);
            c
        } else {
            let mut c = Command::new(&parts[0]);
            if parts.len() > 1 {
                c.args(&parts[1..]);
            }
            c
        };

        command
            .current_dir(cwd)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;
        let mut pumps = JoinSet::new();
        if let Some(stdout) = child.stdout.take() {
            pumps.spawn(pump(stdout, sink.clone(), false));
        }
        if let Some(stderr) = child.stderr.take() {
            pumps.spawn(pump(stderr, sink.clone(), true));
        }
        let status = child.wait().await?;
        while pumps.join_next().await.is_some() {}

        if !status.success() {
            return Err(YatrError::CommandFailed {
                command: cmd.to_string(),
                code: exit_code(status),
            });
        }
        Ok(())
    }

    /// Parse a command string into parts
    fn parse_command(cmd: &str, use_shell: bool) -> Vec<String> {
        if use_shell {
            return vec![cmd.to_string()];
        }

        // Simple shell-like parsing (handles quotes)
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut quote_char = '"';

        for c in cmd.chars() {
            match c {
                '"' | '\'' if !in_quotes => {
                    in_quotes = true;
                    quote_char = c;
                }
                c if c == quote_char && in_quotes => {
                    in_quotes = false;
                }
                ' ' if !in_quotes => {
                    if !current.is_empty() {
                        parts.push(std::mem::take(&mut current));
                    }
                }
                _ => {
                    current.push(c);
                }
            }
        }

        if !current.is_empty() {
            parts.push(current);
        }

        parts
    }

    /// Print dry-run execution plan
    #[allow(clippy::unused_self)]
    fn print_dry_run(&self, plan: &ExecutionPlan) {
        outln!("{}", style("Execution plan (dry run):").bold().cyan());
        outln!();

        for (i, group) in plan.parallel_groups.iter().enumerate() {
            let parallel_note = if group.len() > 1 { " (parallel)" } else { "" };
            outln!(
                "{} {}{}",
                style(format!("Stage {}:", i + 1)).bold(),
                group
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                style(parallel_note).dim()
            );

            for task in group {
                if !task.config.run.is_empty() {
                    for cmd in &task.config.run {
                        outln!("    {} {}", style("→").dim(), cmd);
                    }
                } else if task.config.script.is_some() {
                    outln!(
                        "    {} {}",
                        style("→").dim(),
                        style("[rhai script]").italic()
                    );
                }
            }
        }
    }

    /// Print result of a single task
    fn print_task_result(result: &TaskResult) {
        let status = if result.success {
            if result.cached {
                style("✓ cached").green()
            } else {
                style("✓").green()
            }
        } else {
            style("✗").red()
        };

        let duration = format!("{:.2}s", result.duration.as_secs_f64());

        outln!(
            "{} {} {}",
            status,
            style(&result.name).bold(),
            style(duration).dim()
        );

        if let Some(error) = &result.error {
            errln!("  {}", style(error).red());
        }
    }

    /// Print execution summary
    #[allow(clippy::unused_self)]
    fn print_summary(&self, results: &[TaskResult]) {
        outln!();

        let total: Duration = results.iter().map(|r| r.duration).sum();
        let succeeded = results.iter().filter(|r| r.success).count();
        let failed = results.iter().filter(|r| !r.success).count();
        let cached = results.iter().filter(|r| r.cached).count();

        if failed == 0 {
            outln!(
                "{} {} tasks completed in {:.2}s ({} cached)",
                style("✓").green().bold(),
                succeeded,
                total.as_secs_f64(),
                cached
            );
        } else {
            outln!(
                "{} {} succeeded, {} failed in {:.2}s",
                style("✗").red().bold(),
                succeeded,
                failed,
                total.as_secs_f64()
            );
        }
    }
}

/// A process's exit code, with death by signal N reported as 128 + N like a shell.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_command() {
        let parts = Executor::parse_command("cargo test --all", false);
        assert_eq!(parts, vec!["cargo", "test", "--all"]);
    }

    #[test]
    fn test_parse_command_with_quotes() {
        let parts = Executor::parse_command(r#"echo "hello world""#, false);
        assert_eq!(parts, vec!["echo", "hello world"]);
    }

    #[tokio::test]
    async fn ready_queue_runs_full_diamond_dag() {
        // a → {b, c} → d. All four must run, each after its dependencies.
        let toml = r#"
            [tasks.a]
            run = ["true"]
            [tasks.b]
            depends = ["a"]
            run = ["true"]
            [tasks.c]
            depends = ["a"]
            run = ["true"]
            [tasks.d]
            depends = ["b", "c"]
            run = ["true"]
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        let graph = TaskGraph::from_config(&config).unwrap();
        let exec_config = ExecutorConfig {
            json: true, // suppress human output in tests
            ..Default::default()
        };
        let executor = Executor::new(config, exec_config, None);

        let results = executor.execute(&graph, "d").await.unwrap();
        let names: HashSet<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, HashSet::from(["a", "b", "c", "d"]));
        assert!(results.iter().all(|r| r.success));

        // d must complete after both b and c started no earlier than a finished:
        // verify d is the last to start (largest start_offset).
        let latest = results.iter().max_by_key(|r| r.start_offset).unwrap();
        assert_eq!(latest.name, "d");
    }
}
