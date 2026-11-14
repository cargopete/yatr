//! CLI command definitions and handling
//!
//! Uses `clap` derive API for argument parsing.

use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::report::Mode;

/// YATR - Yet Another Task Runner for Rust projects
#[derive(Parser, Debug)]
#[command(name = "yatr")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Path to yatr.toml config file
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Print each command before it runs
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Print only task output and errors, no status lines or summary
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Working directory
    #[arg(long, global = true)]
    pub cwd: Option<PathBuf>,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Tasks to run (shorthand for `yatr run <task>...`)
    pub task: Vec<String>,

    #[command(flatten)]
    pub run: RunArgs,
}

/// Options shared by `yatr run <task>...` and the bare `yatr <task>...` form.
#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    /// Show execution plan without running
    #[arg(long)]
    pub dry_run: bool,

    /// Force run even if cached
    #[arg(short, long)]
    pub force: bool,

    /// Number of parallel tasks (0 = `[settings] parallelism`, else CPU count)
    #[arg(short, long, default_value = "0")]
    pub parallel: usize,

    /// Use shell to execute commands
    #[arg(long)]
    pub shell: bool,

    /// Emit machine-readable JSON instead of human output
    #[arg(long)]
    pub json: bool,

    /// Write a Chrome trace of the run to this path (open in `chrome://tracing`)
    #[arg(long, value_name = "PATH")]
    pub profile: Option<PathBuf>,

    /// Only run tasks affected by changes since this git ref
    #[arg(long, value_name = "GIT_REF")]
    pub affected: Option<String>,

    /// Warn when a task writes files outside its declared `outputs`
    #[arg(long)]
    pub trace_io: bool,

    /// After a failure, keep running tasks that do not depend on it
    #[arg(short, long)]
    pub keep_going: bool,

    /// Report for a coding agent: a line per task and, for a failure, its
    /// command, exit code and output tail. Also set by `YATR_AGENT=1`
    #[arg(long, conflicts_with_all = ["json", "ndjson"])]
    pub agent: bool,

    /// Emit one JSON event per line as the run progresses
    #[arg(long, conflicts_with = "json")]
    pub ndjson: bool,

    /// Keep the last N lines of each task's output in `--json`, `--ndjson`
    /// and `--agent`; the whole of it goes to `.yatr/logs/<task>.log`
    #[arg(long, value_name = "LINES")]
    pub tail: Option<usize>,
}

impl RunArgs {
    /// How this run reports itself. `YATR_AGENT` asks for the agent report
    /// unless a JSON form was asked for by name.
    pub fn mode(&self) -> Mode {
        let env_agent = std::env::var("YATR_AGENT")
            .is_ok_and(|v| !matches!(v.as_str(), "" | "0" | "false" | "no" | "off"));
        if self.json {
            Mode::Json
        } else if self.ndjson {
            Mode::Ndjson
        } else if self.agent || env_agent {
            Mode::Agent
        } else {
            Mode::Human
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run one or more tasks
    Run {
        /// Tasks to run (lists available tasks if none specified)
        #[arg(required = false)]
        tasks: Vec<String>,

        #[command(flatten)]
        opts: RunArgs,
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

    /// Print the JSON Schema for yatr.toml (for editor validation/autocomplete)
    Schema,

    /// Run the yatr.toml language server (LSP over stdio)
    Lsp,

    /// List tasks affected by changes since a git ref
    Affected {
        /// Git ref to compare against (e.g. `main`, `HEAD~1`, `origin/main...HEAD`)
        git_ref: String,

        /// Output format
        #[arg(short, long, default_value = "table")]
        format: ListFormat,
    },

    /// Say whether a task would run or be restored from cache, and which inputs changed
    Explain {
        /// Task to explain, along with the tasks it depends on
        task: String,

        /// Output format
        #[arg(short, long, default_value = "text")]
        format: ExplainFormat,
    },

    /// Print the AGENTS.md section telling coding agents how to use this project's tasks
    Agents {
        /// Write the section into FILE, replacing an earlier one
        #[arg(long, conflicts_with = "check")]
        write: bool,

        /// Exit non-zero if the section in FILE is missing or out of date
        #[arg(long)]
        check: bool,

        /// The file to write or check (default: AGENTS.md beside yatr.toml)
        file: Option<PathBuf>,
    },

    /// Run a Model Context Protocol server over stdio, with the tasks as tools
    Mcp,
}

#[derive(Subcommand, Debug)]
pub enum CacheCommands {
    /// Show cache statistics
    Stats,

    /// Clear this project's cache entries
    Clear {
        /// Clear cache for specific task only
        task: Option<String>,

        /// Clear entries for every project, not just this one
        #[arg(long)]
        all: bool,
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
pub enum ExplainFormat {
    #[default]
    Text,
    Json,
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
            EffectiveCommand::RunTasks(&self.task, &self.run)
        } else {
            EffectiveCommand::None
        }
    }
}

/// Subcommands added after 1.0. In the bare form a task of the same name
/// still wins over these, as it did before they existed.
pub const YIELDING_BUILTINS: &[&str] = &["explain", "agents", "mcp"];

/// Options that take a value, which is therefore not the first task or subcommand.
const VALUE_OPTIONS: &[&str] = &[
    "-c",
    "--config",
    "--cwd",
    "-p",
    "--parallel",
    "--profile",
    "--affected",
    "--tail",
];

/// Where the first task or subcommand name sits in `args`, if there is one.
pub fn first_positional(args: &[std::ffi::OsString]) -> Option<usize> {
    let mut i = 1;
    while let Some(arg) = args.get(i).and_then(|a| a.to_str()) {
        if arg == "--" {
            return None;
        }
        if !arg.starts_with('-') {
            return Some(i);
        }
        i += if VALUE_OPTIONS.contains(&arg) { 2 } else { 1 };
    }
    None
}

/// Every subcommand name.
pub fn subcommand_names() -> Vec<String> {
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect()
}

/// Subcommand names that win over a task of the same name in the bare form.
pub fn builtin_names() -> Vec<String> {
    let mut names: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .filter(|name| !YIELDING_BUILTINS.contains(&name.as_str()))
        .collect();
    // clap adds `help` when the command is built, so it is not listed above.
    names.push("help".to_string());
    names
}

pub enum EffectiveCommand<'a> {
    Subcommand(&'a Commands),
    RunTasks(&'a [String], &'a RunArgs),
    None,
}
