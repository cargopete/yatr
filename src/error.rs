//! Error types for YATR
//!
//! Uses `miette` for pretty error reporting with source spans and help text.

use miette::Diagnostic;
use std::path::PathBuf;
use thiserror::Error;

/// Main error type for YATR operations
#[derive(Error, Diagnostic, Debug)]
pub enum YatrError {
    #[error("Configuration file not found")]
    #[diagnostic(
        code(yatr::config::not_found),
        help("Create a yatr.toml in your project root, or specify one with --config")
    )]
    ConfigNotFound { searched: Vec<PathBuf> },

    #[error("Failed to parse configuration")]
    #[diagnostic(code(yatr::config::parse))]
    ConfigParse {
        #[source]
        source: toml::de::Error,
        path: PathBuf,
    },

    #[error("Task '{name}' not found{}", did_you_mean(.name, .available))]
    #[diagnostic(
        code(yatr::task::not_found),
        help("Run `yatr list` to see available tasks")
    )]
    TaskNotFound {
        name: String,
        available: Vec<String>,
    },

    #[error("Circular dependency detected: {cycle}")]
    #[diagnostic(
        code(yatr::task::cycle),
        help("Check the 'depends' field in your task definitions")
    )]
    CyclicDependency { cycle: String },

    #[error("Task '{task}' failed with exit code {code}")]
    #[diagnostic(code(yatr::exec::failed))]
    TaskFailed {
        task: String,
        code: i32,
        #[help]
        stderr: Option<String>,
    },

    #[error("{} tasks failed: {}", .tasks.len(), list_failures(.tasks))]
    #[diagnostic(code(yatr::exec::failed))]
    TasksFailed { tasks: Vec<(String, i32)> },

    #[error("Command failed with exit code {code}: {command}")]
    #[diagnostic(code(yatr::exec::command_failed))]
    CommandFailed { command: String, code: i32 },

    #[error("Task '{task}' timed out after {seconds}s")]
    #[diagnostic(code(yatr::exec::timeout))]
    TimedOut { task: String, seconds: u64 },

    #[error("cancelled")]
    #[diagnostic(code(yatr::exec::cancelled))]
    Cancelled,

    #[error("Interrupted by signal {signal}")]
    #[diagnostic(code(yatr::exec::interrupted))]
    Interrupted { signal: i32 },

    #[error("Command not found: {command}")]
    #[diagnostic(
        code(yatr::exec::command_not_found),
        help("Ensure the command is installed and in your PATH")
    )]
    CommandNotFound { command: String },

    #[error("Script execution failed in task '{task}'")]
    #[diagnostic(code(yatr::script::failed))]
    ScriptFailed {
        task: String,
        #[source]
        source: Box<rhai::EvalAltResult>,
    },

    #[error("Invalid task configuration")]
    #[diagnostic(code(yatr::config::invalid_task))]
    InvalidTask { task: String, reason: String },

    #[error("Invalid configuration: {message}")]
    #[diagnostic(code(yatr::config::invalid))]
    InvalidConfig { message: String },

    #[error("WASM plugin error in task '{task}': {message}")]
    #[diagnostic(code(yatr::plugin))]
    Plugin { task: String, message: String },

    #[error("Toolchain '{tool}': {message}")]
    #[diagnostic(code(yatr::toolchain))]
    Toolchain { tool: String, message: String },

    #[error("{message}")]
    #[diagnostic(code(yatr::affected))]
    Affected { message: String },

    #[error("{message}")]
    #[diagnostic(code(yatr::agents::stale))]
    AgentsStale { message: String },

    #[error("I/O error")]
    #[diagnostic(code(yatr::io))]
    Io(#[from] std::io::Error),

    #[error("Cache error: {message}")]
    #[diagnostic(code(yatr::cache))]
    Cache { message: String },

    #[error("Watch error")]
    #[diagnostic(code(yatr::watch))]
    Watch {
        #[source]
        source: notify::Error,
    },
}

impl YatrError {
    /// The process exit code yatr should finish with for this error.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::TaskFailed { code, .. } | Self::CommandFailed { code, .. } => {
                u8::try_from(*code).ok().filter(|c| *c != 0).unwrap_or(1)
            }
            Self::TimedOut { .. } => 124,
            Self::Interrupted { signal } => u8::try_from(128 + signal).unwrap_or(1),
            _ => 1,
        }
    }
}

impl YatrError {
    /// The error as a JSON object, for `--json` and `--format json` output.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::json;

        let mut obj = serde_json::Map::new();
        if let Some(code) = Diagnostic::code(self) {
            obj.insert("code".into(), json!(code.to_string()));
        }
        obj.insert("message".into(), json!(self.to_string()));
        if let Some(help) = Diagnostic::help(self) {
            obj.insert("help".into(), json!(help.to_string()));
        }
        let mut causes = Vec::new();
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            causes.push(cause.to_string());
            source = cause.source();
        }
        if !causes.is_empty() {
            obj.insert("causes".into(), json!(causes));
        }

        let extra = match self {
            Self::ConfigNotFound { searched } => json!({ "searched": searched }),
            Self::ConfigParse { path, .. } => json!({ "path": path }),
            Self::TaskNotFound { name, available } => {
                let mut available = available.clone();
                available.sort_unstable();
                json!({
                    "task": name,
                    "suggestions": suggestions(name, &available),
                    "available": available,
                })
            }
            Self::TaskFailed { task, code, .. } => json!({ "task": task, "exit_code": code }),
            Self::TasksFailed { tasks } => json!({
                "tasks": tasks
                    .iter()
                    .map(|(task, code)| json!({ "task": task, "exit_code": code }))
                    .collect::<Vec<_>>(),
            }),
            Self::CommandFailed { command, code } => {
                json!({ "command": command, "exit_code": code })
            }
            Self::TimedOut { task, seconds } => json!({ "task": task, "seconds": seconds }),
            Self::Interrupted { signal } => json!({ "signal": signal }),
            Self::CommandNotFound { command } => json!({ "command": command }),
            Self::ScriptFailed { task, .. } | Self::Plugin { task, .. } => json!({ "task": task }),
            Self::InvalidTask { task, reason } => json!({ "task": task, "reason": reason }),
            _ => json!({}),
        };
        if let serde_json::Value::Object(extra) = extra {
            obj.extend(extra);
        }
        serde_json::Value::Object(obj)
    }

    /// A failure of the tasks themselves, which the run has already reported.
    #[must_use]
    pub const fn is_task_failure(&self) -> bool {
        matches!(self, Self::TaskFailed { .. } | Self::TasksFailed { .. })
    }
}

/// Task names close enough to `name` to be what was meant, nearest first.
#[must_use]
pub fn suggestions(name: &str, available: &[String]) -> Vec<String> {
    let limit = match name.chars().count() {
        0..=3 => 1,
        4..=8 => 2,
        _ => 3,
    };
    let mut close: Vec<(usize, &String)> = available
        .iter()
        .filter_map(|candidate| {
            let distance = edit_distance(name, candidate);
            let prefixed = name.len() >= 2
                && (candidate.starts_with(name) || name.starts_with(candidate.as_str()));
            (distance <= limit || prefixed).then_some((distance, candidate))
        })
        .collect();
    close.sort();
    close.into_iter().take(3).map(|(_, c)| c.clone()).collect()
}

fn did_you_mean(name: &str, available: &[String]) -> String {
    match suggestions(name, available).as_slice() {
        [] => String::new(),
        close => format!(
            "; did you mean {}?",
            close
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(" or ")
        ),
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = diagonal + usize::from(ca != *cb);
            diagonal = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

fn list_failures(tasks: &[(String, i32)]) -> String {
    tasks
        .iter()
        .map(|(task, code)| format!("{task} (exit code {code})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Result type alias for YATR operations
pub type Result<T> = std::result::Result<T, YatrError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_are_the_near_misses() {
        let available: Vec<String> = ["build", "test", "test-coverage", "lint"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(suggestions("tset", &available), ["test"]);
        assert_eq!(suggestions("tes", &available), ["test", "test-coverage"]);
        assert!(suggestions("deploy", &available).is_empty());
    }

    #[test]
    fn not_found_names_the_near_miss() {
        let e = YatrError::TaskNotFound {
            name: "biuld".into(),
            available: vec!["build".into(), "test".into()],
        };
        assert_eq!(
            e.to_string(),
            "Task 'biuld' not found; did you mean `build`?"
        );
        assert_eq!(e.to_json()["suggestions"][0], "build");
        assert_eq!(e.to_json()["code"], "yatr::task::not_found");
    }
}
