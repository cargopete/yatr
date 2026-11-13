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

    #[error("Task '{name}' not found")]
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

fn list_failures(tasks: &[(String, i32)]) -> String {
    tasks
        .iter()
        .map(|(task, code)| format!("{task} (exit code {code})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Result type alias for YATR operations
pub type Result<T> = std::result::Result<T, YatrError>;
