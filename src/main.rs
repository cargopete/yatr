//! YATR - A modern task runner for Rust projects
//!
//! Fast, ergonomic task automation with:
//! - Simple TOML configuration
//! - Rhai scripting for complex logic
//! - Content-addressable caching
//! - File watching
//! - Parallel execution

#![allow(
    unused_variables,
    unused_imports,
    dead_code,
    unused_assignments,
    mismatched_lifetime_syntaxes,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

use std::process::ExitCode;

use clap::Parser;
use console::style;
use miette::IntoDiagnostic;

mod cache;
mod cli;
mod config;
mod error;
mod executor;
mod graph;
mod script;
mod watch;

use cli::{CacheCommands, Cli, Commands, EffectiveCommand, GraphFormat, ListFormat};
use config::Config;
use error::{Result, YatrError};
use executor::{Executor, ExecutorConfig, TaskResult};
use graph::TaskGraph;

#[tokio::main]
async fn main() -> ExitCode {
    // Set up panic handler for nice error messages
    miette::set_panic_hook();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .without_time()
        .init();

    let cli = Cli::parse();

    // Handle --no-color
    if cli.no_color {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {:?}", style("error").red().bold(), e);
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    // Change working directory if specified
    if let Some(cwd) = &cli.cwd {
        std::env::set_current_dir(cwd)?;
    }

    match cli.effective_command() {
        EffectiveCommand::Subcommand(cmd) => run_command(cmd, &cli).await,
        EffectiveCommand::RunTasks(tasks) => {
            run_tasks(tasks, false, false, 0, false, false, &cli).await
        }
        EffectiveCommand::None => {
            // No command - show help or list tasks
            let (config, _) = Config::load(cli.config.as_deref())?;
            let graph = TaskGraph::from_config(&config)?;
            print_task_list(&graph, &config, &ListFormat::Table, false);
            Ok(())
        }
    }
}

async fn run_command(cmd: &Commands, cli: &Cli) -> Result<()> {
    match cmd {
        Commands::Run {
            tasks,
            dry_run,
            force,
            parallel,
            shell,
            json,
        } => {
            if tasks.is_empty() {
                let (config, _) = Config::load(cli.config.as_deref())?;
                let graph = TaskGraph::from_config(&config)?;
                print_task_list(&graph, &config, &ListFormat::Table, false);
                Ok(())
            } else {
                run_tasks(tasks, *dry_run, *force, *parallel, *shell, *json, cli).await
            }
        }

        Commands::List { format, deps } => {
            let (config, _) = Config::load(cli.config.as_deref())?;
            let graph = TaskGraph::from_config(&config)?;
            print_task_list(&graph, &config, format, *deps);
            Ok(())
        }

        Commands::Watch { task, clear } => {
            let (config, _) = Config::load(cli.config.as_deref())?;
            let graph = TaskGraph::from_config(&config)?;

            let exec_config = ExecutorConfig {
                verbose: cli.verbose,
                cwd: std::env::current_dir()?,
                ..Default::default()
            };

            watch::watch_and_run(&config, &graph, task, exec_config).await
        }

        Commands::Graph { task, format } => {
            let (config, _) = Config::load(cli.config.as_deref())?;
            let graph = TaskGraph::from_config(&config)?;
            print_graph(&graph, task.as_deref(), format)?;
            Ok(())
        }

        Commands::Cache { command } => run_cache_command(command, cli).await,

        Commands::Init { force } => init_config(*force),

        Commands::Check => {
            let (config, path) = Config::load(cli.config.as_deref())?;
            let graph = TaskGraph::from_config(&config)?;

            println!(
                "{} {} is valid ({} tasks)",
                style("✓").green(),
                path.display(),
                graph.task_names().count()
            );
            Ok(())
        }

        Commands::Schema => {
            let schema = schemars::schema_for!(Config);
            let json = serde_json::to_string_pretty(&schema)
                .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))?;
            println!("{json}");
            Ok(())
        }
    }
}

#[allow(clippy::fn_params_excessive_bools)]
async fn run_tasks(
    tasks: &[String],
    dry_run: bool,
    force: bool,
    parallel: usize,
    shell: bool,
    json: bool,
    cli: &Cli,
) -> Result<()> {
    let (config, _) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;

    // JSON dry-run: emit the execution plan rather than running anything.
    if json && dry_run {
        let mut plan = Vec::new();
        for task in tasks {
            let order: Vec<&str> = graph
                .execution_order(task)?
                .iter()
                .map(|t| t.name.as_str())
                .collect();
            plan.push(serde_json::json!({ "task": task, "order": order }));
        }
        print_json(&serde_json::json!({ "plan": plan }))?;
        return Ok(());
    }

    let cache = if config.settings.cache && !dry_run {
        Some(cache::Cache::new(config.settings.cache_dir.clone())?)
    } else {
        None
    };

    let exec_config = ExecutorConfig {
        parallelism: parallel,
        dry_run,
        force,
        cwd: std::env::current_dir()?,
        shell,
        verbose: cli.verbose,
        json,
    };

    let executor = Executor::new(config, exec_config, cache);

    let mut all_results = Vec::new();
    for task in tasks {
        let mut results = executor.execute(&graph, task).await?;
        all_results.append(&mut results);
    }

    if json {
        print_run_json(&all_results)?;
    }

    Ok(())
}

/// Serialize a run's task results into the structured `--json` document.
fn print_run_json(results: &[TaskResult]) -> Result<()> {
    let ms = |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);

    let tasks: Vec<_> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "success": r.success,
                "cached": r.cached,
                "duration_ms": ms(r.duration),
                "output": r.output,
                "error": r.error,
            })
        })
        .collect();

    let doc = serde_json::json!({
        "tasks": tasks,
        "summary": {
            "succeeded": results.iter().filter(|r| r.success).count(),
            "failed": results.iter().filter(|r| !r.success).count(),
            "cached": results.iter().filter(|r| r.cached).count(),
            "duration_ms": results.iter().map(|r| ms(r.duration)).sum::<u64>(),
        }
    });

    print_json(&doc)
}

/// Print a JSON value to stdout, pretty-printed.
fn print_json(value: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))?;
    println!("{text}");
    Ok(())
}

async fn run_cache_command(cmd: &CacheCommands, cli: &Cli) -> Result<()> {
    let cache_dir = cli.config.as_ref().and_then(|_| {
        Config::load(cli.config.as_deref())
            .ok()
            .and_then(|(c, _)| c.settings.cache_dir)
    });

    let cache = cache::Cache::new(cache_dir)?;

    match cmd {
        CacheCommands::Stats => {
            let stats = cache.stats()?;
            println!("Cache: {stats}");
        }

        CacheCommands::Clear { task } => {
            if let Some(task) = task {
                let removed = cache.clear_task(task)?;
                println!(
                    "{} Cleared {removed} cache {} for task '{task}'",
                    style("✓").green(),
                    if removed == 1 { "entry" } else { "entries" },
                );
            } else {
                cache.clear().await?;
                println!("{} Cache cleared", style("✓").green());
            }
        }

        CacheCommands::Path => {
            let stats = cache.stats()?;
            println!("{}", stats.cache_dir.display());
        }
    }

    Ok(())
}

fn print_task_list(graph: &TaskGraph, config: &Config, format: &ListFormat, show_deps: bool) {
    match format {
        ListFormat::Table => {
            println!("{}", style("Available tasks:").bold());
            println!();

            let mut names: Vec<_> = graph.task_names().collect();
            names.sort_unstable();

            let max_name_len = names.iter().map(|n| n.len()).max().unwrap_or(0);

            for name in names {
                if let Some(task) = graph.get_task(name) {
                    let desc = task.config.desc.as_deref().unwrap_or("");

                    print!(
                        "  {}{}  {}",
                        style(name).cyan().bold(),
                        " ".repeat(max_name_len - name.len()),
                        style(desc).dim()
                    );

                    if show_deps {
                        if let Some(deps) = graph.dependencies(name) {
                            if !deps.is_empty() {
                                print!(
                                    " {}",
                                    style(format!("[deps: {}]", deps.join(", "))).yellow().dim()
                                );
                            }
                        }
                    }

                    println!();
                }
            }
        }

        ListFormat::Json => {
            let mut tasks = serde_json::Map::new();
            for name in graph.task_names() {
                if let Some(task) = graph.get_task(name) {
                    let mut obj = serde_json::Map::new();
                    if let Some(desc) = &task.config.desc {
                        obj.insert("description".to_string(), serde_json::json!(desc));
                    }
                    if show_deps {
                        if let Some(deps) = graph.dependencies(name) {
                            obj.insert("depends".to_string(), serde_json::json!(deps));
                        }
                    }
                    tasks.insert(name.to_string(), serde_json::Value::Object(obj));
                }
            }
            println!("{}", serde_json::to_string_pretty(&tasks).unwrap());
        }

        ListFormat::Plain => {
            let mut names: Vec<_> = graph.task_names().collect();
            names.sort_unstable();
            for name in names {
                println!("{name}");
            }
        }
    }
}

fn print_graph(graph: &TaskGraph, task: Option<&str>, format: &GraphFormat) -> Result<()> {
    let tasks = if let Some(name) = task {
        graph.execution_order(name)?
    } else {
        graph.all_tasks_ordered()?
    };

    match format {
        GraphFormat::Text => {
            println!("{}", style("Task dependency graph:").bold());
            println!();

            for task_node in &tasks {
                let deps = graph.dependencies(&task_node.name).unwrap_or_default();

                if deps.is_empty() {
                    println!("  {}", style(&task_node.name).cyan().bold());
                } else {
                    println!(
                        "  {} {} {}",
                        style(&task_node.name).cyan().bold(),
                        style("←").dim(),
                        deps.join(", ")
                    );
                }
            }
        }

        GraphFormat::Dot => {
            println!("digraph yatr {{");
            println!("  rankdir=LR;");
            println!("  node [shape=box];");

            for task_node in &tasks {
                if let Some(deps) = graph.dependencies(&task_node.name) {
                    for dep in deps {
                        println!("  \"{}\" -> \"{}\";", dep, task_node.name);
                    }
                }
            }

            println!("}}");
        }

        GraphFormat::Json => {
            let mut nodes = Vec::new();
            let mut edges = Vec::new();

            for task_node in &tasks {
                nodes.push(serde_json::json!({
                    "id": task_node.name,
                    "description": task_node.config.desc,
                }));

                if let Some(deps) = graph.dependencies(&task_node.name) {
                    for dep in deps {
                        edges.push(serde_json::json!({
                            "from": dep,
                            "to": task_node.name,
                        }));
                    }
                }
            }

            let output = serde_json::json!({
                "nodes": nodes,
                "edges": edges,
            });

            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
    }

    Ok(())
}

fn init_config(force: bool) -> Result<()> {
    let path = std::path::Path::new("YATR.toml");

    if path.exists() && !force {
        return Err(YatrError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "YATR.toml already exists (use --force to overwrite)",
        )));
    }

    let template = r#"# YATR.toml - Task runner configuration
# See https://github.com/yourusername/yatr for documentation

[env]
# Global environment variables
# RUST_LOG = "debug"

[settings]
# cache = true              # Enable task caching
# parallelism = 0           # Max parallel tasks (0 = auto)
# watch_debounce_ms = 300   # Watch mode debounce delay

[tasks.fmt]
desc = "Format code"
run = ["cargo fmt"]

[tasks.lint]
desc = "Run clippy"
run = ["cargo clippy -- -D warnings"]

[tasks.test]
desc = "Run tests"
run = ["cargo test"]

[tasks.check]
desc = "Format, lint, and test"
depends = ["fmt", "lint", "test"]
run = []

[tasks.build]
desc = "Build release binary"
depends = ["check"]
run = ["cargo build --release"]
"#;

    std::fs::write(path, template)?;

    println!(
        "{} Created {}",
        style("✓").green(),
        style("YATR.toml").bold()
    );

    Ok(())
}
