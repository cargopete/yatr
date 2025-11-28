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
use tokio::time::timeout;

use crate::cache::Cache;
use crate::config::{Config, TaskConfig};
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
            self.print_dry_run(&plan);
            return Ok(Vec::new());
        }

        let parallelism = if self.exec_config.parallelism == 0 {
            num_cpus::get()
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
                    let _permit = sem.acquire().await.unwrap();

                    let pb = mp.add(ProgressBar::new_spinner());
                    pb.set_style(
                        ProgressStyle::default_spinner()
                            .template("{spinner:.cyan} {msg}")
                            .unwrap(),
                    );
                    pb.set_message(format!("Running {}", task_clone.name));
                    pb.enable_steady_tick(Duration::from_millis(100));

                    let result = Self::execute_single_task(
                        &task_clone,
                        &config,
                        &exec_config,
                        cache.as_ref(),
                    )
                    .await;

                    pb.finish_and_clear();
                    result
                });

                handles.push(handle);
            }

            // Wait for all tasks in this group
            for handle in handles {
                let result = handle.await.map_err(|e| YatrError::Io(
                    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                ))??;

                let success = result.success;
                let task_name = result.name.clone();
                let allow_failure = graph
                    .get_task(&task_name)
                    .map(|t| t.config.allow_failure)
                    .unwrap_or(false);

                Self::print_task_result(&result);
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

        self.print_summary(&all_results);
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

        // Check cache
        if !exec_config.force {
            if let Some(cache) = cache {
                if !task.config.no_cache {
                    if let Some(cached) = cache.get(&task.name, &task.config).await? {
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

        // Determine working directory
        let cwd = task
            .config
            .cwd
            .clone()
            .unwrap_or_else(|| exec_config.cwd.clone());

        let result = if let Some(script) = &task.config.script {
            // Execute Rhai script
            Self::execute_script(&task.name, script, &env, &cwd).await
        } else if task.config.parallel {
            // Execute commands in parallel
            Self::execute_commands_parallel(&task.name, &task.config.run, &env, &cwd, exec_config)
                .await
        } else {
            // Execute commands sequentially
            Self::execute_commands_sequential(&task.name, &task.config.run, &env, &cwd, exec_config)
                .await
        };

        let duration = start.elapsed();

        match result {
            Ok(output) => {
                // Store in cache
                if let Some(cache) = cache {
                    if !task.config.no_cache {
                        let _ = cache.put(&task.name, &task.config, &output).await;
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
    async fn execute_script(
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
        task_name: &str,
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

    /// Execute commands in parallel
    async fn execute_commands_parallel(
        task_name: &str,
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
            let output = handle.await.map_err(|e| YatrError::Io(
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
            ))??;
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
                    println!("    {} {}", style("→").dim(), style("[rhai script]").italic());
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
    }

    /// Print execution summary
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

// num_cpus isn't in our deps, so let's add a simple fallback
mod num_cpus {
    pub fn get() -> usize {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
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
