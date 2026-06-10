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

mod affected;
mod cache;
mod cli;
mod config;
mod error;
mod executor;
mod graph;
mod lsp;
mod remote;
mod script;
mod toolchain;
mod trace;
mod wasm;
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
        EffectiveCommand::RunTasks(tasks) => run_tasks(tasks, RunOpts::default(), &cli).await,
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
            profile,
            affected,
            trace_io,
        } => {
            if tasks.is_empty() {
                let (config, _) = Config::load(cli.config.as_deref())?;
                let graph = TaskGraph::from_config(&config)?;
                print_task_list(&graph, &config, &ListFormat::Table, false);
                Ok(())
            } else {
                let opts = RunOpts {
                    dry_run: *dry_run,
                    force: *force,
                    parallel: *parallel,
                    shell: *shell,
                    json: *json,
                    profile: profile.clone(),
                    affected: affected.clone(),
                    trace_io: *trace_io,
                };
                run_tasks(tasks, opts, cli).await
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

        Commands::Check => run_check_command(cli),

        Commands::Schema => {
            let schema = schemars::schema_for!(Config);
            let json = serde_json::to_string_pretty(&schema)
                .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))?;
            println!("{json}");
            Ok(())
        }

        Commands::Affected { git_ref, format } => run_affected_command(git_ref, format, cli),

        // The LSP runs a synchronous stdio loop; keep it off the async executor.
        Commands::Lsp => tokio::task::block_in_place(lsp::run),
    }
}

fn run_check_command(cli: &Cli) -> Result<()> {
    let (config, path) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for name in graph.task_names() {
        let Some(task) = config.get_task(name) else {
            continue;
        };
        let base = task.cwd.clone().unwrap_or_else(|| ".".into());

        // Referenced paths must exist.
        if let Some(cwd) = &task.cwd {
            if !cwd.is_dir() {
                errors.push(format!(
                    "task '{name}': cwd '{}' does not exist",
                    cwd.display()
                ));
            }
        }
        if let Some(wasm) = &task.wasm {
            let wasm_str = wasm.to_string_lossy();
            if !wasm::is_remote_ref(&wasm_str) {
                let resolved = if wasm.is_absolute() {
                    wasm.clone()
                } else {
                    base.join(wasm)
                };
                if !resolved.is_file() {
                    errors.push(format!(
                        "task '{name}': wasm plugin '{}' not found",
                        resolved.display()
                    ));
                }
            }
        }

        // Config smells worth a nudge.
        if !task.outputs.is_empty() && task.no_cache {
            warnings.push(format!(
                "task '{name}': declares `outputs` but `no_cache = true` — outputs won't be cached"
            ));
        }
        if task.foreground && task.run.len() > 1 {
            warnings.push(format!(
                "task '{name}': foreground tasks only run their first command ({} given)",
                task.run.len()
            ));
        }
    }

    for w in &warnings {
        println!("{} {w}", style("warning:").yellow().bold());
    }
    for e in &errors {
        println!("{} {e}", style("error:").red().bold());
    }

    if errors.is_empty() {
        let suffix = if warnings.is_empty() {
            String::new()
        } else {
            format!(", {} warning(s)", warnings.len())
        };
        println!(
            "{} {} is valid ({} tasks{suffix})",
            style("✓").green(),
            path.display(),
            graph.task_names().count()
        );
        Ok(())
    } else {
        Err(YatrError::InvalidConfig {
            message: format!("{} problem(s) found in {}", errors.len(), path.display()),
        })
    }
}

fn run_affected_command(git_ref: &str, format: &ListFormat, cli: &Cli) -> Result<()> {
    let (config, _) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;
    let changed = affected::changed_files(git_ref)?;
    let affected = affected::affected_tasks(&graph, &changed);

    let mut names: Vec<&str> = affected.iter().map(String::as_str).collect();
    names.sort_unstable();

    match format {
        ListFormat::Json => print_json(&serde_json::json!({
            "ref": git_ref,
            "changed_files": changed,
            "affected": names,
        }))?,
        ListFormat::Plain => {
            for name in names {
                println!("{name}");
            }
        }
        ListFormat::Table => {
            if names.is_empty() {
                println!("{} No tasks affected since {git_ref}", style("✓").green());
            } else {
                println!(
                    "{}",
                    style(format!("Affected by changes since {git_ref}:")).bold()
                );
                println!();
                for name in names {
                    println!("  {}", style(name).cyan().bold());
                }
            }
        }
    }
    Ok(())
}

/// Options for a `run` invocation.
#[derive(Default)]
struct RunOpts {
    dry_run: bool,
    force: bool,
    parallel: usize,
    shell: bool,
    json: bool,
    profile: Option<std::path::PathBuf>,
    affected: Option<String>,
    trace_io: bool,
}

async fn run_tasks(tasks: &[String], opts: RunOpts, cli: &Cli) -> Result<()> {
    let (mut config, _) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;

    // Ensure pinned toolchains are installed and put them on the task PATH.
    if !config.toolchain.is_empty() && !opts.dry_run {
        let bins = toolchain::ensure_all(&config.toolchain, &toolchain::toolchains_dir()).await?;
        if !bins.is_empty() {
            let mut paths = bins;
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(&paths) {
                config
                    .env
                    .insert("PATH".to_string(), joined.to_string_lossy().into_owned());
            }
        }
    }

    // --affected: keep only the requested tasks that changes since the ref touch.
    let filtered: Vec<String>;
    let tasks: &[String] = if let Some(git_ref) = &opts.affected {
        let changed = affected::changed_files(git_ref)?;
        let set = affected::affected_tasks(&graph, &changed);
        filtered = tasks.iter().filter(|t| set.contains(*t)).cloned().collect();
        if filtered.is_empty() && !opts.json {
            println!(
                "{} No requested tasks affected since {git_ref} — nothing to do",
                style("✓").green()
            );
        }
        &filtered
    } else {
        tasks
    };

    // JSON dry-run: emit the execution plan rather than running anything.
    if opts.json && opts.dry_run {
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

    let cache = if config.settings.cache && !opts.dry_run {
        let remote_cfg = config.settings.remote_cache.as_ref();
        let remote = match remote_cfg {
            Some(rc) => Some(remote::RemoteCache::from_config(rc)?),
            None => None,
        };
        let signing_key = remote_cfg
            .and_then(|rc| rc.sign_key_env.as_ref())
            .and_then(|var| std::env::var(var).ok())
            .map(|secret| cache::Cache::derive_key(&secret));
        Some(
            cache::Cache::new(config.settings.cache_dir.clone())?
                .with_remote(remote)
                .with_signing_key(signing_key),
        )
    } else {
        None
    };

    let exec_config = ExecutorConfig {
        parallelism: opts.parallel,
        dry_run: opts.dry_run,
        force: opts.force,
        cwd: std::env::current_dir()?,
        shell: opts.shell,
        verbose: cli.verbose,
        json: opts.json,
        trace_io: opts.trace_io,
        run_start: std::time::Instant::now(),
    };

    let executor = Executor::new(config, exec_config, cache);

    let mut all_results = Vec::new();
    for task in tasks {
        let mut results = executor.execute(&graph, task).await?;
        all_results.append(&mut results);
    }

    if opts.json {
        print_run_json(&all_results)?;
    }
    if let Some(path) = &opts.profile {
        write_profile(&all_results, path)?;
        if !opts.json {
            println!("{} Wrote trace to {}", style("⏱").cyan(), path.display());
        }
    }

    Ok(())
}

/// Write a Chrome Trace Event Format file (viewable in `chrome://tracing` or
/// Perfetto): one complete event per task, placed on the run timeline.
fn write_profile(results: &[TaskResult], path: &std::path::Path) -> Result<()> {
    let us = |d: std::time::Duration| u64::try_from(d.as_micros()).unwrap_or(u64::MAX);

    let events: Vec<_> = results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            serde_json::json!({
                "name": r.name,
                "cat": if r.cached { "cached" } else { "run" },
                "ph": "X",
                "pid": 1,
                "tid": i + 1,
                "ts": us(r.start_offset),
                "dur": us(r.duration),
                "args": { "success": r.success, "cached": r.cached },
            })
        })
        .collect();

    let doc = serde_json::json!({
        "traceEvents": events,
        "displayTimeUnit": "ms",
    });
    let text = serde_json::to_string(&doc)
        .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))?;
    std::fs::write(path, text)?;
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
