//! File watching for automatic task re-execution
//!
//! Uses `notify` crate with debouncing to watch for file changes
//! and trigger task re-runs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{Event, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebouncedEvent, Debouncer};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::error::{Result, YatrError};
use crate::executor::{Executor, ExecutorConfig};
use crate::graph::TaskGraph;

/// File watcher for tasks
pub struct TaskWatcher {
    /// Debounced watcher
    debouncer: Debouncer<notify::RecommendedWatcher>,
    /// Receive channel for events
    rx: mpsc::Receiver<Vec<PathBuf>>,
    /// Glob patterns to watch
    patterns: GlobSet,
    /// Task to re-run
    task_name: String,
}

impl TaskWatcher {
    /// Create a new watcher for a task
    pub fn new(
        task_name: &str,
        patterns: &[String],
        debounce_ms: u64,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel(16);

        // Build glob set
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|e| YatrError::Watch {
                source: notify::Error::generic(&format!("Invalid glob '{}': {}", pattern, e)),
            })?;
            builder.add(glob);
        }
        let patterns = builder.build().map_err(|e| YatrError::Watch {
            source: notify::Error::generic(&format!("Failed to build glob set: {}", e)),
        })?;

        // Create debounced watcher
        let tx_clone = tx.clone();
        let debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            move |events: std::result::Result<Vec<DebouncedEvent>, notify::Error>| {
                if let Ok(events) = events {
                    let paths: Vec<PathBuf> = events
                        .into_iter()
                        .map(|e| e.path)
                        .collect();
                    let _ = tx_clone.blocking_send(paths);
                }
            },
        )
        .map_err(|e| YatrError::Watch { source: e })?;

        Ok(Self {
            debouncer,
            rx,
            patterns,
            task_name: task_name.to_string(),
        })
    }

    /// Start watching paths
    pub fn watch(&mut self, paths: &[PathBuf]) -> Result<()> {
        for path in paths {
            self.debouncer
                .watcher()
                .watch(path, RecursiveMode::Recursive)
                .map_err(|e| YatrError::Watch { source: e })?;
        }
        Ok(())
    }

    /// Wait for the next relevant file change
    pub async fn wait_for_change(&mut self) -> Option<Vec<PathBuf>> {
        loop {
            let paths = self.rx.recv().await?;

            // Filter to only matching paths
            let matching: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| self.patterns.is_match(p))
                .collect();

            if !matching.is_empty() {
                return Some(matching);
            }
        }
    }

    /// Get the task name being watched
    pub fn task_name(&self) -> &str {
        &self.task_name
    }
}

/// Run a task in watch mode
pub async fn watch_and_run(
    config: &Config,
    graph: &TaskGraph,
    task_name: &str,
    exec_config: ExecutorConfig,
) -> Result<()> {
    use console::style;

    let task = graph.get_task(task_name).ok_or_else(|| YatrError::TaskNotFound {
        name: task_name.to_string(),
        available: graph.task_names().map(|s| s.to_string()).collect(),
    })?;

    // Determine watch patterns
    let patterns = if task.config.watch.is_empty() {
        // Default: watch source files if specified, otherwise watch common patterns
        if task.config.sources.is_empty() {
            vec![
                "**/*.rs".to_string(),
                "**/*.toml".to_string(),
                "Cargo.lock".to_string(),
            ]
        } else {
            task.config.sources.clone()
        }
    } else {
        task.config.watch.clone()
    };

    println!(
        "{} Watching for changes to run task '{}'",
        style("👀").cyan(),
        style(task_name).bold()
    );
    println!(
        "   Patterns: {}",
        style(patterns.join(", ")).dim()
    );
    println!();

    // Initial run
    let executor = Executor::new(
        config.clone(),
        exec_config.clone(),
        None, // Disable cache in watch mode for now
    );

    println!("{}", style("─".repeat(60)).dim());
    let _ = executor.execute(graph, task_name).await;
    println!("{}", style("─".repeat(60)).dim());

    // Set up watcher
    let mut watcher = TaskWatcher::new(
        task_name,
        &patterns,
        config.settings.watch_debounce_ms,
    )?;

    // Watch current directory
    watcher.watch(&[std::env::current_dir()?])?;

    // Watch loop
    loop {
        if let Some(changed) = watcher.wait_for_change().await {
            println!();
            println!(
                "{} Changed: {}",
                style("📝").yellow(),
                changed
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            // Clear screen option could go here
            println!("{}", style("─".repeat(60)).dim());

            let executor = Executor::new(
                config.clone(),
                exec_config.clone(),
                None,
            );

            let _ = executor.execute(graph, task_name).await;
            println!("{}", style("─".repeat(60)).dim());
            println!(
                "{} Waiting for changes...",
                style("👀").cyan()
            );
        }
    }
}

/// Collect all watch patterns from a task and its dependencies
pub fn collect_watch_patterns(graph: &TaskGraph, task_name: &str) -> Result<Vec<String>> {
    let tasks = graph.execution_order(task_name)?;
    let mut patterns = HashSet::new();

    for task in tasks {
        for pattern in &task.config.watch {
            patterns.insert(pattern.clone());
        }
        for pattern in &task.config.sources {
            patterns.insert(pattern.clone());
        }
    }

    // Add default patterns if none specified
    if patterns.is_empty() {
        patterns.insert("**/*.rs".to_string());
        patterns.insert("**/Cargo.toml".to_string());
    }

    Ok(patterns.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_patterns() {
        let config: Config = toml::from_str(
            r#"
            [tasks.test]
            run = ["cargo test"]
            watch = ["src/**/*.rs", "tests/**/*.rs"]
            sources = ["Cargo.toml"]
            "#,
        )
        .unwrap();

        let graph = TaskGraph::from_config(&config).unwrap();
        let patterns = collect_watch_patterns(&graph, "test").unwrap();

        assert!(patterns.contains(&"src/**/*.rs".to_string()));
        assert!(patterns.contains(&"tests/**/*.rs".to_string()));
        assert!(patterns.contains(&"Cargo.toml".to_string()));
    }
}
