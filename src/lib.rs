//! YATR - A fast, polyglot, single-binary task runner.
//!
//! This crate provides both a CLI tool and a library for task automation.
//!
//! # Features
//!
//! - **Simple TOML configuration** - Define tasks in a familiar format
//! - **Rhai scripting** - Embedded scripting for complex task logic
//! - **Content-addressable caching** - Skip unchanged tasks
//! - **File watching** - Re-run tasks on file changes
//! - **Parallel execution** - Run independent tasks concurrently
//! - **Dependency resolution** - Automatic task ordering
//!
//! # Example
//!
//! ```toml
//! # YATR.toml
//!
//! [tasks.test]
//! desc = "Run all tests"
//! run = ["cargo test --all-targets"]
//!
//! [tasks.build]
//! desc = "Build release"
//! depends = ["test"]
//! run = ["cargo build --release"]
//! ```
//!
//! # Library Usage
//!
//! ```rust,ignore
//! use yatr::{Config, TaskGraph, Executor, ExecutorConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let (config, _) = Config::load(None)?;
//!     let graph = TaskGraph::from_config(&config)?;
//!     
//!     let executor = Executor::new(config, ExecutorConfig::default(), None);
//!     executor.execute(&graph, "build").await?;
//!     
//!     Ok(())
//! }
//! ```

#![allow(
    dead_code,
    unused,
    unused_variables,
    unused_imports,
    unused_assignments,
    mismatched_lifetime_syntaxes,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::struct_excessive_bools
)]

pub mod affected;
pub mod cache;
pub mod config;
pub mod error;
pub mod executor;
pub mod globs;
pub mod graph;
pub mod lsp;
mod output;
pub mod reapi;
pub mod remote;
pub mod script;
pub mod toolchain;
pub mod trace;
pub mod wasm;
pub mod watch;

// Re-export main types
pub use cache::Cache;
pub use config::Config;
pub use error::{Result, YatrError};
pub use executor::{Executor, ExecutorConfig, TaskResult};
pub use graph::{ExecutionPlan, TaskGraph, TaskNode};
pub use remote::RemoteCache;
pub use script::ScriptEngine;
