//! File watching for automatic task re-execution
//!
//! Uses `notify` crate with debouncing to watch for file changes
//! and trigger task re-runs.
#![allow(clippy::missing_errors_doc)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use notify::RecursiveMode;
use notify_debouncer_mini::{new_debouncer, DebouncedEvent, Debouncer};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::error::{Result, YatrError};
use crate::executor::{Executor, ExecutorConfig};
use crate::graph::TaskGraph;

/// Patterns used when no task in the watched chain declares `watch` or `sources`.
const DEFAULT_PATTERNS: &[&str] = &["**/*.rs", "**/*.toml", "Cargo.lock"];

/// One task's watch patterns and the directory they are relative to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchScope {
    /// Directory the patterns resolve against (the task's `cwd`).
    pub base: PathBuf,
    /// Glob patterns, relative to `base`.
    pub patterns: Vec<String>,
}

struct CompiledScope {
    base: PathBuf,
    globs: GlobSet,
}

/// File watcher for tasks
pub struct TaskWatcher {
    /// Debounced watcher
    debouncer: Debouncer<notify::RecommendedWatcher>,
    /// Receive channel for events
    rx: mpsc::Receiver<Vec<PathBuf>>,
    scopes: Vec<CompiledScope>,
    /// Task to re-run
    task_name: String,
}

fn watch_err(message: &str) -> YatrError {
    YatrError::Watch {
        source: notify::Error::generic(message),
    }
}

/// Canonicalise where possible: notify reports resolved paths (on macOS,
/// `/private/var/...` for `/var/...`), so the bases must be resolved too.
fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn compile(scope: &WatchScope) -> Result<CompiledScope> {
    let mut builder = GlobSetBuilder::new();
    for pattern in &scope.patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| watch_err(&format!("Invalid glob '{pattern}': {e}")))?;
        builder.add(glob);
    }
    let globs = builder
        .build()
        .map_err(|e| watch_err(&format!("Failed to build glob set: {e}")))?;
    Ok(CompiledScope {
        base: resolve(&scope.base),
        globs,
    })
}

impl CompiledScope {
    fn is_match(&self, path: &Path) -> bool {
        path.strip_prefix(&self.base)
            .is_ok_and(|rel| self.globs.is_match(rel))
    }
}

impl TaskWatcher {
    /// Create a new watcher for a task
    pub fn new(task_name: &str, scopes: &[WatchScope], debounce_ms: u64) -> Result<Self> {
        let (tx, rx) = mpsc::channel(16);
        let scopes = scopes.iter().map(compile).collect::<Result<Vec<_>>>()?;

        let debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            move |events: std::result::Result<Vec<DebouncedEvent>, notify::Error>| {
                if let Ok(events) = events {
                    let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                    let _ = tx.blocking_send(paths);
                }
            },
        )
        .map_err(|e| YatrError::Watch { source: e })?;

        Ok(Self {
            debouncer,
            rx,
            scopes,
            task_name: task_name.to_string(),
        })
    }

    /// Start watching every scope's base directory, skipping any nested in
    /// another (a recursive watch already covers them).
    pub fn watch_scopes(&mut self) -> Result<()> {
        let mut roots: Vec<PathBuf> = self.scopes.iter().map(|s| s.base.clone()).collect();
        roots.sort();
        roots.dedup();
        let roots: Vec<PathBuf> = roots
            .iter()
            .filter(|r| !roots.iter().any(|o| o != *r && r.starts_with(o)))
            .cloned()
            .collect();
        self.watch(&roots)
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

    /// Whether an absolute path reported by notify matches any scope.
    #[must_use]
    pub fn is_relevant(&self, path: &Path) -> bool {
        self.scopes.iter().any(|s| s.is_match(path))
    }

    /// Wait for the next relevant file change
    pub async fn wait_for_change(&mut self) -> Option<Vec<PathBuf>> {
        loop {
            let paths = self.rx.recv().await?;

            let matching: Vec<PathBuf> =
                paths.into_iter().filter(|p| self.is_relevant(p)).collect();

            if !matching.is_empty() {
                return Some(matching);
            }
        }
    }

    /// Get the task name being watched
    #[must_use]
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
    clear: bool,
) -> Result<()> {
    use console::style;

    if !graph.has_task(task_name) {
        return Err(YatrError::TaskNotFound {
            name: task_name.to_string(),
            available: graph
                .task_names()
                .map(std::string::ToString::to_string)
                .collect(),
        });
    }

    let scopes = collect_watch_scopes(graph, task_name, &exec_config.cwd)?;
    let mut watcher = TaskWatcher::new(task_name, &scopes, config.settings.watch_debounce_ms)?;
    watcher.watch_scopes()?;

    let clear_screen = || {
        if clear {
            let _ = console::Term::stdout().clear_screen();
        }
    };

    clear_screen();
    println!(
        "{} Watching for changes to run task '{}'",
        style("👀").cyan(),
        style(task_name).bold()
    );
    let mut shown: Vec<&str> = scopes
        .iter()
        .flat_map(|s| s.patterns.iter().map(String::as_str))
        .collect();
    shown.dedup();
    println!("   Patterns: {}", style(shown.join(", ")).dim());
    println!();

    // The cache stays off in watch mode: a re-run is what was asked for.
    let executor = Executor::new(config.clone(), exec_config.clone(), None);

    println!("{}", style("─".repeat(60)).dim());
    let _ = executor.execute(graph, task_name).await;
    println!("{}", style("─".repeat(60)).dim());

    loop {
        if let Some(changed) = watcher.wait_for_change().await {
            clear_screen();
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
            println!("{}", style("─".repeat(60)).dim());

            let _ = executor.execute(graph, task_name).await;
            println!("{}", style("─".repeat(60)).dim());
            println!("{} Waiting for changes...", style("👀").cyan());
        }
    }
}

/// Collect the watch scope of a task and each of its dependencies: a task's
/// `watch` patterns if set, else its `sources`, relative to its `cwd`.
pub fn collect_watch_scopes(
    graph: &TaskGraph,
    task_name: &str,
    root: &Path,
) -> Result<Vec<WatchScope>> {
    let mut scopes = Vec::new();
    for task in graph.execution_order(task_name)? {
        let patterns = if task.config.watch.is_empty() {
            &task.config.sources
        } else {
            &task.config.watch
        };
        if patterns.is_empty() {
            continue;
        }
        let base = task
            .config
            .cwd
            .as_ref()
            .map_or_else(|| root.to_path_buf(), |c| root.join(c));
        scopes.push(WatchScope {
            base,
            patterns: patterns.clone(),
        });
    }

    if scopes.is_empty() {
        let base = graph
            .get_task(task_name)
            .and_then(|t| t.config.cwd.as_ref())
            .map_or_else(|| root.to_path_buf(), |c| root.join(c));
        scopes.push(WatchScope {
            base,
            patterns: DEFAULT_PATTERNS.iter().map(ToString::to_string).collect(),
        });
    }

    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(toml: &str) -> TaskGraph {
        let config: Config = toml::from_str(toml).unwrap();
        TaskGraph::from_config(&config).unwrap()
    }

    #[test]
    fn scopes_cover_dependencies_and_respect_cwd() {
        let g = graph(
            r#"
            [tasks.gen]
            run = ["echo gen"]
            cwd = "web"
            sources = ["src/**"]

            [tasks.test]
            run = ["cargo test"]
            depends = ["gen"]
            watch = ["src/**/*.rs"]
            sources = ["Cargo.toml"]
            "#,
        );
        let root = Path::new("/repo");
        let scopes = collect_watch_scopes(&g, "test", root).unwrap();
        assert_eq!(
            scopes,
            vec![
                WatchScope {
                    base: root.join("web"),
                    patterns: vec!["src/**".into()],
                },
                WatchScope {
                    base: root.to_path_buf(),
                    patterns: vec!["src/**/*.rs".into()],
                },
            ]
        );
    }

    #[test]
    fn relative_patterns_match_absolute_event_paths() {
        let scope = compile(&WatchScope {
            base: PathBuf::from("/repo/web"),
            patterns: vec!["src/*.txt".into()],
        })
        .unwrap();
        assert!(scope.is_match(Path::new("/repo/web/src/a.txt")));
        assert!(!scope.is_match(Path::new("/repo/web/src/nested/a.txt")));
        assert!(!scope.is_match(Path::new("/repo/src/a.txt")));
    }

    #[test]
    fn defaults_apply_when_nothing_is_declared() {
        let g = graph("[tasks.t]\nrun = [\"true\"]\n");
        let scopes = collect_watch_scopes(&g, "t", Path::new("/repo")).unwrap();
        assert_eq!(scopes.len(), 1);
        assert!(scopes[0].patterns.contains(&"**/*.rs".to_string()));
    }
}
