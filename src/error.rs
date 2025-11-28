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
    ConfigNotFound {
        searched: Vec<PathBuf>,
    },

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
    CyclicDependency {
        cycle: String,
    },

    #[error("Task '{task}' failed with exit code {code}")]
    #[diagnostic(code(yatr::exec::failed))]
    TaskFailed {
        task: String,
        code: i32,
        #[help]
        stderr: Option<String>,
    },

    #[error("Command not found: {command}")]
    #[diagnostic(
        code(yatr::exec::command_not_found),
        help("Ensure the command is installed and in your PATH")
    )]
    CommandNotFound {
        command: String,
    },

    #[error("Script execution failed in task '{task}'")]
    #[diagnostic(code(yatr::script::failed))]
    ScriptFailed {
        task: String,
        #[source]
        source: Box<rhai::EvalAltResult>,
    },

    #[error("Invalid task configuration")]
    #[diagnostic(code(yatr::config::invalid_task))]
    InvalidTask {
        task: String,
        reason: String,
    },

    #[error("I/O error")]
    #[diagnostic(code(yatr::io))]
    Io(#[from] std::io::Error),

    #[error("Cache error: {message}")]
    #[diagnostic(code(yatr::cache))]
    Cache {
        message: String,
    },

    #[error("Watch error")]
    #[diagnostic(code(yatr::watch))]
    Watch {
        #[source]
        source: notify::Error,
    },
}

/// Result type alias for YATR operations
pub type Result<T> = std::result::Result<T, YatrError>;
