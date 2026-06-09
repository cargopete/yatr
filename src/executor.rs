//! Task execution engine
//!
//! Handles running commands, managing parallel execution, and coordinating
//! with the cache and scripting systems.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::cache::Cache;
use crate::config::Config;
use crate::error::{Result, YatrError};
use crate::graph::{ExecutionPlan, TaskGraph, TaskNode};
use crate::script::ScriptEngine;

/// Result of executing a single task
#[derive(Debug)]
pub struct TaskResult {
    pub name: String,
    pub success: bool,
    pub duration: Duration,
    pub cached: bool,
    pub output: Option<String>,
    pub error: Option<String>,
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
        let tasks = graph.execution_order(task_name)?;
        let plan = ExecutionPlan::from_tasks(tasks, graph);

        if self.exec_config.dry_run {
            if !self.exec_config.json {
                self.print_dry_run(&plan);
            }
            return Ok(Vec::new());
        }

        let parallelism = if self.exec_config.parallelism == 0 {
            std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
        } else {
            self.exec_config.parallelism
        };

        let semaphore = Arc::new(Semaphore::new(parallelism));
        let multi_progress = MultiProgress::new();
        let mut all_results = Vec::new();

        // Execute groups sequentially, tasks within groups in parallel
        for group in &plan.parallel_groups {
            let mut handles = Vec::new();

            for task in group {
                let task_clone = (*task).clone();
                let config = Arc::clone(&self.config);
                let sem = Arc::clone(&semaphore);
                let exec_config = self.exec_config.clone();
                let cache = self.cache.clone();
                let mp = multi_progress.clone();

                let handle = tokio::spawn(async move {
                    let _permit = match sem.acquire().await {
                        Ok(p) => p,
                        Err(e) => {
                            return Err(YatrError::Io(std::io::Error::other(format!(
                                "Semaphore acquire failed: {e}"
                            ))))
                        }
                    };

                    // Progress spinner is human-only; suppress it in JSON mode.
                    let pb = (!exec_config.json).then(|| {
                        let pb = mp.add(ProgressBar::new_spinner());
                        let style = ProgressStyle::default_spinner()
                            .template("{spinner:.cyan} {msg}")
                            .unwrap_or_else(|_| ProgressStyle::default_spinner());
                        pb.set_style(style);
                        pb.set_message(format!("Running {}", task_clone.name));
                        pb.enable_steady_tick(Duration::from_millis(100));
                        pb
                    });

                    let result = Self::execute_single_task(
                        &task_clone,
                        &config,
                        &exec_config,
                        cache.as_ref(),
                    )
                    .await;

                    if let Some(pb) = pb {
                        pb.finish_and_clear();
                    }
                    result
                });

                handles.push(handle);
            }

            // Wait for all tasks in this group
            for handle in handles {
                let result = handle
                    .await
                    .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))??;

                let success = result.success;
                let task_name = result.name.clone();
                let allow_failure = graph
                    .get_task(&task_name)
                    .is_some_and(|t| t.config.allow_failure);

                if !self.exec_config.json {
                    Self::print_task_result(&result);
                }
                all_results.push(result);

                if !success && !allow_failure {
                    return Err(YatrError::TaskFailed {
                        task: task_name,
                        code: 1,
                        stderr: None,
                    });
                }
            }
        }

        if !self.exec_config.json {
            self.print_summary(&all_results);
        }
        Ok(all_results)
    }

    /// Execute a single task
    async fn execute_single_task(
        task: &TaskNode,
        config: &Config,
        exec_config: &ExecutorConfig,
        cache: Option<&Cache>,
    ) -> Result<TaskResult> {
        let start = Instant::now();
        let env = config.task_env(&task.config);

        // Determine working directory (needed for cache key + output restore)
        let cwd = task
            .config
            .cwd
            .clone()
            .unwrap_or_else(|| exec_config.cwd.clone());

        // Check cache
        if !exec_config.force {
            if let Some(cache) = cache {
                if !task.config.no_cache {
                    if let Some(cached) = cache.get(&task.name, &task.config, &cwd).await? {
                        return Ok(TaskResult {
                            name: task.name.clone(),
                            success: true,
                            duration: start.elapsed(),
                            cached: true,
                            output: Some(cached),
                            error: None,
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

        let result = if task.config.foreground {
            // Execute in foreground with inherited stdio (for long-running processes)
            Self::execute_foreground(&task.name, &task.config.run, &env, &cwd, &task_exec_config)
                .await
        } else if let Some(script) = &task.config.script {
            // Execute Rhai script
            Self::execute_script(&task.name, script, &env, &cwd)
        } else if task.config.parallel {
            // Execute commands in parallel
            Self::execute_commands_parallel(
                &task.name,
                &task.config.run,
                &env,
                &cwd,
                &task_exec_config,
            )
            .await
        } else {
            // Execute commands sequentially
            Self::execute_commands_sequential(
                &task.name,
                &task.config.run,
                &env,
                &cwd,
                &task_exec_config,
            )
            .await
        };

        let duration = start.elapsed();

        match result {
            Ok(output) => {
                // Store in cache (skip foreground tasks: their output isn't captured)
                if let Some(cache) = cache {
                    if !task.config.no_cache && !task.config.foreground {
                        let _ = cache
                            .put(&task.name, &task.config, &cwd, &output, duration)
                            .await;
                    }
                }

                Ok(TaskResult {
                    name: task.name.clone(),
                    success: true,
                    duration,
                    cached: false,
                    output: Some(output),
                    error: None,
                })
            }
            Err(e) => Ok(TaskResult {
                name: task.name.clone(),
                success: false,
                duration,
                cached: false,
                output: None,
                error: Some(e.to_string()),
            }),
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

    /// Execute commands sequentially
    async fn execute_commands_sequential(
        _task_name: &str,
        commands: &[String],
        env: &HashMap<String, String>,
        cwd: &Path,
        exec_config: &ExecutorConfig,
    ) -> Result<String> {
        let mut all_output = String::new();

        for cmd in commands {
            let output = Self::execute_command(cmd, env, cwd, exec_config).await?;
            all_output.push_str(&output);
            all_output.push('\n');
        }

        Ok(all_output)
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
            return Err(YatrError::TaskFailed {
                task: cmd.clone(),
                code: status.code().unwrap_or(1),
                stderr: None,
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
    ) -> Result<String> {
        let mut handles = Vec::new();

        for cmd in commands {
            let cmd = cmd.clone();
            let env = env.clone();
            let cwd = cwd.to_path_buf();
            let exec_config = exec_config.clone();

            handles.push(tokio::spawn(async move {
                Self::execute_command(&cmd, &env, &cwd, &exec_config).await
            }));
        }

        let mut all_output = String::new();
        for handle in handles {
            let output = handle
                .await
                .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))??;
            all_output.push_str(&output);
            all_output.push('\n');
        }

        Ok(all_output)
    }

    /// Execute a single command
    async fn execute_command(
        cmd: &str,
        env: &HashMap<String, String>,
        cwd: &Path,
        exec_config: &ExecutorConfig,
    ) -> Result<String> {
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = command.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(YatrError::TaskFailed {
                task: cmd.to_string(),
                code: output.status.code().unwrap_or(1),
                stderr: Some(stderr.to_string()),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.to_string())
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
        println!("{}", style("Execution plan (dry run):").bold().cyan());
        println!();

        for (i, group) in plan.parallel_groups.iter().enumerate() {
            let parallel_note = if group.len() > 1 { " (parallel)" } else { "" };
            println!(
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
                        println!("    {} {}", style("→").dim(), cmd);
                    }
                } else if task.config.script.is_some() {
                    println!(
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

        println!(
            "{} {} {}",
            status,
            style(&result.name).bold(),
            style(duration).dim()
        );

        if let Some(error) = &result.error {
            eprintln!("  {}", style(error).red());
        }

        // Print command output if present
        if let Some(output) = &result.output {
            let trimmed = output.trim();
            if !trimmed.is_empty() {
                for line in trimmed.lines() {
                    println!("  {line}");
                }
            }
        }
    }

    /// Print execution summary
    #[allow(clippy::unused_self)]
    fn print_summary(&self, results: &[TaskResult]) {
        println!();

        let total: Duration = results.iter().map(|r| r.duration).sum();
        let succeeded = results.iter().filter(|r| r.success).count();
        let failed = results.iter().filter(|r| !r.success).count();
        let cached = results.iter().filter(|r| r.cached).count();

        if failed == 0 {
            println!(
                "{} {} tasks completed in {:.2}s ({} cached)",
                style("✓").green().bold(),
                succeeded,
                total.as_secs_f64(),
                cached
            );
        } else {
            println!(
                "{} {} succeeded, {} failed in {:.2}s",
                style("✗").red().bold(),
                succeeded,
                failed,
                total.as_secs_f64()
            );
        }
    }
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
}
