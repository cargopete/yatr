//! CLI command definitions and handling
//!
//! Uses `clap` derive API for argument parsing.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// YATR - Yet Another Task Runner for Rust projects
#[derive(Parser, Debug)]
#[command(name = "yatr")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    /// Path to yatr.toml config file
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Suppress output (quiet mode)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Working directory
    #[arg(long, global = true)]
    pub cwd: Option<PathBuf>,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Task to run (shorthand for `yatr run <task>`)
    #[arg(trailing_var_arg = true)]
    pub task: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run one or more tasks
    Run {
        /// Tasks to run
        #[arg(required = true)]
        tasks: Vec<String>,

        /// Show execution plan without running
        #[arg(long)]
        dry_run: bool,

        /// Force run even if cached
        #[arg(short, long)]
        force: bool,

        /// Number of parallel tasks (0 = auto)
        #[arg(short, long, default_value = "0")]
        parallel: usize,

        /// Use shell to execute commands
        #[arg(long)]
        shell: bool,
    },

    /// List available tasks
    List {
        /// Output format
        #[arg(short, long, default_value = "table")]
        format: ListFormat,

        /// Show task dependencies
        #[arg(long)]
        deps: bool,
    },

    /// Watch for file changes and re-run task
    Watch {
        /// Task to run on changes
        task: String,

        /// Clear screen before each run
        #[arg(long)]
        clear: bool,
    },

    /// Show task dependency graph
    Graph {
        /// Task to show graph for (all tasks if not specified)
        task: Option<String>,

        /// Output format
        #[arg(short, long, default_value = "text")]
        format: GraphFormat,
    },

    /// Manage the task cache
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },

    /// Initialize a new yatr.toml
    Init {
        /// Overwrite existing config
        #[arg(short, long)]
        force: bool,
    },

    /// Validate yatr.toml configuration
    Check,
}

#[derive(Subcommand, Debug)]
pub enum CacheCommands {
    /// Show cache statistics
    Stats,

    /// Clear the cache
    Clear {
        /// Clear cache for specific task only
        task: Option<String>,
    },

    /// Show cache directory location
    Path,
}

#[derive(ValueEnum, Clone, Debug, Default)]
pub enum ListFormat {
    #[default]
    Table,
    Json,
    Plain,
}

#[derive(ValueEnum, Clone, Debug, Default)]
pub enum GraphFormat {
    #[default]
    Text,
    Dot,
    Json,
}

impl Cli {
    /// Get the effective command, treating bare task names as `run <task>`
    pub fn effective_command(&self) -> EffectiveCommand {
        if let Some(cmd) = &self.command {
            EffectiveCommand::Subcommand(cmd)
        } else if !self.task.is_empty() {
            EffectiveCommand::RunTasks(&self.task)
        } else {
            EffectiveCommand::None
        }
    }
}

pub enum EffectiveCommand<'a> {
    Subcommand(&'a Commands),
    RunTasks(&'a Vec<String>),
    None,
}
