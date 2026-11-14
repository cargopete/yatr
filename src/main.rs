//! YATR - A fast, polyglot, single-binary task runner.
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
    clippy::missing_panics_doc,
    clippy::struct_excessive_bools
)]

use std::process::ExitCode;

use clap::Parser;
use console::style;
use miette::IntoDiagnostic;

mod affected;
mod agents;
mod cache;
mod cli;
mod config;
mod error;
mod executor;
mod explain;
mod globs;
mod graph;
mod lsp;
mod mcp;
mod output;
mod reapi;
mod remote;
mod report;
mod script;
mod toolchain;
mod trace;
mod wasm;
mod watch;

use cache::Cache;
use cli::{
    CacheCommands, Cli, Commands, EffectiveCommand, ExplainFormat, GraphFormat, ListFormat, RunArgs,
};
use config::Config;
use error::{Result, YatrError};
use executor::{Executor, ExecutorConfig, TaskResult};
use graph::TaskGraph;
use output::{errln, out, outln};
use report::Mode;

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

    let cli = Cli::parse_from(args_with_tasks_first());

    // Handle --no-color
    if cli.no_color {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }

    let mode = report_mode(&cli);
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let code = e.exit_code();
            // A JSON reader gets the error as JSON, unless the run's own
            // document already carries it.
            if matches!(mode, Mode::Json | Mode::Ndjson) && !report::emitted() {
                report::error_document(&e, mode);
            }
            // The agent report has already said which tasks failed and why.
            if !(mode == Mode::Agent && e.is_task_failure()) {
                // Render via miette for a clean message + diagnostic code/help,
                // rather than the raw `{:?}` struct debug.
                errln!("{:?}", miette::Report::new(e));
            }
            ExitCode::from(code)
        }
    }
}

/// The command line as clap should see it. Clap only takes a subcommand as
/// the first argument, so two things are put right here:
///
/// - `yatr -q list` has its subcommand moved to the front. It used to fail
///   looking for a task called `list`; where there is such a task, that is
///   still what runs.
/// - A task named after a subcommand added since 1.0 gets `run` put in front,
///   so `yatr <task>` goes on reaching the task as it did before.
fn args_with_tasks_first() -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some(at) = cli::first_positional(&args) else {
        return args;
    };
    let name = args[at].to_string_lossy().into_owned();
    let yields = cli::YIELDING_BUILTINS.contains(&name.as_str());
    if !cli::subcommand_names().contains(&name) || (at == 1 && !yields) {
        return args;
    }

    // Only the options ahead of the name can say where the config is.
    let option = |names: [&str; 2]| {
        args[..at].iter().enumerate().find_map(|(i, arg)| {
            let arg = arg.to_str()?;
            if names.contains(&arg) {
                return args.get(i + 1).map(std::path::PathBuf::from);
            }
            let (flag, value) = arg.split_once('=')?;
            names.contains(&flag).then(|| value.into())
        })
    };
    let config = option(["-c", "--config"]);
    let config = match (option(["--cwd", "--cwd"]), config) {
        (Some(cwd), Some(config)) => Some(cwd.join(config)),
        (Some(cwd), None) => config::CONFIG_FILES
            .iter()
            .map(|name| cwd.join(name))
            .find(|path| path.exists()),
        (None, config) => config,
    };
    let is_task =
        Config::load(config.as_deref()).is_ok_and(|(config, _)| config.tasks.contains_key(&name));

    if is_task && yields {
        args.insert(1, "run".into());
    } else if !is_task && at > 1 {
        let subcommand = args.remove(at);
        args.insert(1, subcommand);
    }
    args
}

/// How errors are reported: as JSON to a command that was asked for JSON.
fn report_mode(cli: &Cli) -> Mode {
    match cli.effective_command() {
        EffectiveCommand::RunTasks(_, opts)
        | EffectiveCommand::Subcommand(Commands::Run { opts, .. }) => opts.mode(),
        EffectiveCommand::Subcommand(
            Commands::List {
                format: ListFormat::Json,
                ..
            }
            | Commands::Affected {
                format: ListFormat::Json,
                ..
            }
            | Commands::Graph {
                format: GraphFormat::Json,
                ..
            }
            | Commands::Explain {
                format: ExplainFormat::Json,
                ..
            },
        ) => Mode::Json,
        _ => Mode::Human,
    }
}

async fn run(cli: Cli) -> Result<()> {
    // Change working directory if specified
    if let Some(cwd) = &cli.cwd {
        std::env::set_current_dir(cwd)?;
    }

    match cli.effective_command() {
        EffectiveCommand::Subcommand(cmd) => run_command(cmd, &cli).await,
        EffectiveCommand::RunTasks(tasks, opts) => run_tasks(tasks, opts, &cli).await,
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
        Commands::Run { tasks, opts } => {
            if tasks.is_empty() {
                let (config, _) = Config::load(cli.config.as_deref())?;
                let graph = TaskGraph::from_config(&config)?;
                print_task_list(&graph, &config, &ListFormat::Table, false);
                Ok(())
            } else {
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
            let (mut config, _) = Config::load(cli.config.as_deref())?;
            let graph = TaskGraph::from_config(&config)?;
            apply_toolchains(&mut config).await?;

            let exec_config = ExecutorConfig {
                verbose: cli.verbose,
                quiet: cli.quiet,
                cwd: std::env::current_dir()?,
                ..Default::default()
            };

            watch::watch_and_run(&config, &graph, task, exec_config, *clear).await
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
            outln!("{json}");
            Ok(())
        }

        Commands::Affected { git_ref, format } => run_affected_command(git_ref, format, cli),

        Commands::Explain { task, format } => run_explain_command(task, format, cli),

        Commands::Agents { write, check, file } => {
            run_agents_command(*write, *check, file.as_deref(), cli)
        }

        Commands::Mcp => mcp::serve(cli.config.clone()).await,

        // The LSP runs a synchronous stdio loop; keep it off the async executor.
        Commands::Lsp => tokio::task::block_in_place(lsp::run),
    }
}

fn run_check_command(cli: &Cli) -> Result<()> {
    let (config, path) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    warn_unsupported_expansions("[env]", &config.env, &mut warnings);
    let builtins = cli::builtin_names();
    for name in graph.task_names() {
        if let Some(task) = config.get_task(name) {
            check_task(name, task, &config, &builtins, &mut errors, &mut warnings);
        }
    }

    for w in &warnings {
        outln!("{} {w}", style("warning:").yellow().bold());
    }
    for e in &errors {
        outln!("{} {e}", style("error:").red().bold());
    }

    if errors.is_empty() {
        let suffix = if warnings.is_empty() {
            String::new()
        } else {
            format!(", {} warning(s)", warnings.len())
        };
        let count = graph.task_names().count();
        outln!(
            "{} {} is valid ({count} {}{suffix})",
            style("✓").green(),
            path.display(),
            if count == 1 { "task" } else { "tasks" },
        );
        Ok(())
    } else {
        Err(YatrError::InvalidConfig {
            message: format!("{} problem(s) found in {}", errors.len(), path.display()),
        })
    }
}

/// `${...}` forms that `env` expansion keeps as literal text.
fn warn_unsupported_expansions(
    scope: &str,
    env: &std::collections::HashMap<String, String>,
    warnings: &mut Vec<String>,
) {
    let mut vars: Vec<_> = env.iter().collect();
    vars.sort();
    for (key, value) in vars {
        for form in config::unsupported_expansions(value) {
            warnings.push(format!(
                "{scope}: `{key}` uses `{form}`, which is not expanded; only `${{VAR}}`, \
                 `${{VAR:-default}}` and `${{VAR-default}}` are"
            ));
        }
    }
}

/// The per-task checks behind `yatr check`.
fn check_task(
    name: &str,
    task: &config::TaskConfig,
    config: &Config,
    builtins: &[String],
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if builtins.iter().any(|b| b == name) {
        warnings.push(format!(
            "task '{name}': `yatr {name}` runs the builtin subcommand, not this task; \
             use `yatr run {name}` or rename it"
        ));
    }
    warn_unsupported_expansions(&format!("task '{name}'"), &task.env, warnings);
    let base = task.working_dir().unwrap_or_else(|| ".".into());

    // Referenced paths must exist.
    if task.cwd.is_some() && !base.is_dir() {
        errors.push(format!(
            "task '{name}': cwd '{}' does not exist",
            base.display()
        ));
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
    if !task.outputs.is_empty() && task.sources.is_empty() && !task.no_cache {
        warnings.push(format!(
            "task '{name}': declares `outputs` but no `sources` — a task without sources is never cached"
        ));
    }
    // A `sources` pattern matching nothing contributes nothing to the
    // cache key, so the task silently stops invalidating. Nearly always a
    // typo or a path that moved.
    if !task.sources.is_empty() && !task.no_cache {
        match Cache::unmatched_source_patterns(&base, &task.sources) {
            Ok(unmatched) if !unmatched.is_empty() => {
                warnings.push(format!(
                    "task '{name}': sources pattern(s) match no files: {}. \
                     They contribute nothing to the cache key, so the task \
                     will not re-run when they change",
                    unmatched.join(", ")
                ));
            }
            Ok(_) => {}
            Err(e) => {
                warnings.push(format!("task '{name}': could not check sources: {e}"));
            }
        }
    }

    if !task.shell.unwrap_or(false) {
        for cmd in &task.run {
            if let Some(syntax) = config::shell_syntax(cmd) {
                warnings.push(format!(
                    "task '{name}': `{cmd}` uses shell syntax (`{syntax}`) but the task \
                     does not set `shell = true`, so it is passed to the program literally"
                ));
            }
        }
    }
    let has_commands = !task.run.is_empty() || task.script.is_some() || task.wasm.is_some();
    if config.settings.cache
        && has_commands
        && task.sources.is_empty()
        && !task.no_cache
        && !task.foreground
    {
        warnings.push(format!(
            "task '{name}': declares no `sources`, so yatr cannot tell when its inputs \
             change. Declare `sources`, or set `no_cache = true` if it should always run"
        ));
    }

    if task.foreground && task.run.len() > 1 {
        warnings.push(format!(
            "task '{name}': foreground tasks only run their first command ({} given)",
            task.run.len()
        ));
    }
}

/// The local cache a run of `config` uses, with its remote and signing key.
fn open_cache(config: &Config) -> Result<Cache> {
    let remote_cfg = config.settings.remote_cache.as_ref();
    let remote = match remote_cfg {
        Some(rc) => Some(remote::RemoteCache::from_config(rc)?),
        None => None,
    };
    let signing_key = remote_cfg
        .and_then(|rc| rc.sign_key_env.as_ref())
        .and_then(|var| std::env::var(var).ok())
        .map(|secret| cache::Cache::derive_key(&secret));
    Ok(cache::Cache::new(config.settings.cache_dir.clone())?
        .with_remote(remote)
        .with_signing_key(signing_key))
}

fn run_explain_command(task: &str, format: &ExplainFormat, cli: &Cli) -> Result<()> {
    let (config, _) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;
    let cache = if config.settings.cache {
        Some(open_cache(&config)?)
    } else {
        None
    };
    let explained = explain::explain(
        task,
        &config,
        &graph,
        cache.as_ref(),
        &std::env::current_dir()?,
    )?;
    match format {
        ExplainFormat::Text => out!("{}", explain::render(&explained)),
        ExplainFormat::Json => print_json(&serde_json::json!({
            "task": task,
            "tasks": explained,
        }))?,
    }
    Ok(())
}

fn run_agents_command(
    write: bool,
    check: bool,
    file: Option<&std::path::Path>,
    cli: &Cli,
) -> Result<()> {
    let (config, config_path) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;
    let config_name = config_path
        .file_name()
        .map_or_else(|| "yatr.toml".into(), |n| n.to_string_lossy().into_owned());
    let root = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    // No FILE means AGENTS.md beside the config, wherever yatr was started.
    let path = file.map_or_else(|| root.join("AGENTS.md"), std::path::Path::to_path_buf);
    // The section names its own file, relative to the config, unless it is
    // the default.
    let named = std::path::absolute(&path)
        .ok()
        .and_then(|p| p.strip_prefix(root).ok().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| path.clone());
    let named = named.to_string_lossy().replace('\\', "/");
    let section = agents::section(
        &config,
        &graph,
        &config_name,
        (file.is_some() && named != "AGENTS.md").then_some(named.as_str()),
    );
    if !write && !check {
        out!("{section}");
        return Ok(());
    }

    let path = std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            path.strip_prefix(cwd)
                .ok()
                .map(std::path::Path::to_path_buf)
        })
        .unwrap_or(path);
    if check {
        agents::check(&path, &section)?;
        errln!("{} {} is up to date", style("✓").green(), path.display());
    } else {
        let changed = agents::write(&path, &section)?;
        errln!(
            "{} {} {}",
            style("✓").green(),
            if changed { "Wrote" } else { "No change to" },
            path.display()
        );
    }
    Ok(())
}

/// The directory yatr was started in, in the canonical form config paths use.
fn run_affected_command(git_ref: &str, format: &ListFormat, cli: &Cli) -> Result<()> {
    let (config, config_path) = Config::load(cli.config.as_deref())?;
    let graph = TaskGraph::from_config(&config)?;
    let affected = affected::detect(git_ref, &config, &config_path, &graph)?;

    let mut names: Vec<&str> = affected.tasks.iter().map(String::as_str).collect();
    names.sort_unstable();

    match format {
        ListFormat::Json => print_json(&serde_json::json!({
            "ref": git_ref,
            "merge_base": affected.merge_base,
            "changed_files": affected.changed,
            "affected": names,
        }))?,
        ListFormat::Plain => {
            for name in names {
                outln!("{name}");
            }
        }
        ListFormat::Table => {
            if names.is_empty() {
                outln!("{} No tasks affected since {git_ref}", style("✓").green());
            } else {
                outln!(
                    "{}",
                    style(format!("Affected by changes since {git_ref}:")).bold()
                );
                outln!();
                for name in names {
                    outln!("  {}", style(name).cyan().bold());
                }
            }
        }
    }
    Ok(())
}

/// Ensure pinned toolchains are installed and put them on the task PATH.
async fn apply_toolchains(config: &mut Config) -> Result<()> {
    if config.toolchain.is_empty() {
        return Ok(());
    }
    config.toolchain_bins =
        toolchain::ensure_all(&config.toolchain, &toolchain::toolchains_dir()).await?;
    Ok(())
}

async fn run_tasks(tasks: &[String], opts: &RunArgs, cli: &Cli) -> Result<()> {
    let mode = opts.mode();
    let (mut config, config_path) = Config::load(cli.config.as_deref())?;
    let mut graph = TaskGraph::from_config(&config)?;
    graph.execution_order_all(tasks)?;

    // --affected: run only affected tasks, dependencies included; unaffected
    // dependencies are dropped from the graph rather than run.
    let filtered: Vec<String>;
    let tasks: &[String] = if let Some(git_ref) = &opts.affected {
        let set = affected::detect(git_ref, &config, &config_path, &graph)?.tasks;
        filtered = tasks.iter().filter(|t| set.contains(*t)).cloned().collect();
        if filtered.is_empty() && mode == Mode::Agent {
            outln!("OK: none of the requested tasks is affected since {git_ref}");
        } else if filtered.is_empty() && mode == Mode::Human && !cli.quiet {
            errln!(
                "{} No requested tasks affected since {git_ref} — nothing to do",
                style("✓").green()
            );
        }
        config.tasks.retain(|name, _| set.contains(name));
        for task in config.tasks.values_mut() {
            task.depends.retain(|d| set.contains(d));
        }
        graph = TaskGraph::from_config(&config)?;
        &filtered
    } else {
        tasks
    };

    if !opts.dry_run {
        apply_toolchains(&mut config).await?;
    }

    // JSON dry-run: emit the execution plan rather than running anything.
    if matches!(mode, Mode::Json | Mode::Ndjson) && opts.dry_run {
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
        Some(open_cache(&config)?)
    } else {
        None
    };

    // The reports that shorten a task's output keep the whole of it on disk.
    let tail = opts
        .tail
        .or_else(|| (mode == Mode::Agent).then_some(report::AGENT_TAIL));
    let log_dir = (tail.is_some() && mode != Mode::Human && !opts.dry_run)
        .then(|| {
            config_path
                .parent()
                .map(|dir| dir.join(".yatr").join("logs"))
        })
        .flatten();

    let exec_config = ExecutorConfig {
        parallelism: opts.parallel,
        dry_run: opts.dry_run,
        force: opts.force,
        cwd: std::env::current_dir()?,
        shell: opts.shell,
        verbose: cli.verbose,
        quiet: cli.quiet,
        // A dry run prints its plan as text for an agent, as for a person.
        json: mode != Mode::Human && !(mode == Mode::Agent && opts.dry_run),
        trace_io: opts.trace_io,
        keep_going: opts.keep_going,
        run_start: std::time::Instant::now(),
        agent: mode == Mode::Agent,
        ndjson: mode == Mode::Ndjson,
        tail,
        log_dir,
    };

    let executor = Executor::new(config, exec_config, cache);
    let mut all_results = Vec::new();
    let outcome = if tasks.is_empty() {
        Ok(())
    } else {
        executor.execute_into(&graph, tasks, &mut all_results).await
    };

    match mode {
        Mode::Json => report::run_document(&all_results, tail, outcome.as_ref().err()),
        Mode::Ndjson => report::event_run_finish(&all_results, outcome.as_ref().err()),
        Mode::Agent | Mode::Human => {}
    }
    if let Some(path) = &opts.profile {
        write_profile(&all_results, path)?;
        if mode == Mode::Human && !cli.quiet {
            errln!("{} Wrote trace to {}", style("⏱").cyan(), path.display());
        }
    }

    outcome
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

/// Print a JSON value to stdout, pretty-printed.
fn print_json(value: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| YatrError::Io(std::io::Error::other(e.to_string())))?;
    outln!("{text}");
    Ok(())
}

async fn run_cache_command(cmd: &CacheCommands, cli: &Cli) -> Result<()> {
    // Cache commands work without a config, so a missing one means the default dir.
    let loaded = Config::load(cli.config.as_deref()).ok();
    let project_root = loaded
        .as_ref()
        .and_then(|(_, path)| path.parent().map(std::path::Path::to_path_buf));
    let cache_dir = loaded.and_then(|(c, _)| c.settings.cache_dir);

    let cache = cache::Cache::new(cache_dir)?;

    match cmd {
        CacheCommands::Stats => {
            let stats = cache.stats()?;
            outln!("Cache: {stats}");
        }

        CacheCommands::Clear { task, all } => {
            if *all && task.is_none() {
                cache.clear().await?;
                outln!("{} Cache cleared for every project", style("✓").green());
                return Ok(());
            }
            let root = if *all {
                None
            } else {
                Some(project_root.ok_or_else(|| {
                    YatrError::InvalidConfig {
                        message: "no yatr.toml here, so there is no project to clear; \
                              pass --all to clear the cache for every project"
                            .to_string(),
                    }
                })?)
            };
            let removed = cache.clear_entries(root.as_deref(), task.as_deref())?;
            let what = task
                .as_ref()
                .map_or_else(String::new, |t| format!(" for task '{t}'"));
            outln!(
                "{} Cleared {removed} cache {}{what}",
                style("✓").green(),
                if removed == 1 { "entry" } else { "entries" },
            );
        }

        CacheCommands::Path => {
            let stats = cache.stats()?;
            outln!("{}", stats.cache_dir.display());
        }
    }

    Ok(())
}

fn print_task_list(graph: &TaskGraph, config: &Config, format: &ListFormat, show_deps: bool) {
    match format {
        ListFormat::Table => {
            outln!("{}", style("Available tasks:").bold());
            outln!();

            let mut names: Vec<_> = graph.task_names().collect();
            names.sort_unstable();

            let max_name_len = names.iter().map(|n| n.len()).max().unwrap_or(0);

            for name in names {
                if let Some(task) = graph.get_task(name) {
                    let desc = task.config.desc.as_deref().unwrap_or("");

                    out!(
                        "  {}{}  {}",
                        style(name).cyan().bold(),
                        " ".repeat(max_name_len - name.len()),
                        style(desc).dim()
                    );

                    if show_deps {
                        if let Some(deps) = graph.dependencies(name) {
                            if !deps.is_empty() {
                                out!(
                                    " {}",
                                    style(format!("[deps: {}]", deps.join(", "))).yellow().dim()
                                );
                            }
                        }
                    }

                    outln!();
                }
            }
        }

        ListFormat::Json => {
            let mut tasks = serde_json::Map::new();
            for name in graph.task_names() {
                if let Some(task) = graph.get_task(name) {
                    let t = &task.config;
                    let kind = if t.wasm.is_some() {
                        "wasm"
                    } else if t.script.is_some() {
                        "script"
                    } else if t.run.is_empty() {
                        "aggregate"
                    } else {
                        "commands"
                    };
                    let mut obj = serde_json::Map::new();
                    if let Some(desc) = &t.desc {
                        obj.insert("description".to_string(), serde_json::json!(desc));
                    }
                    obj.insert(
                        "depends".to_string(),
                        serde_json::json!(graph.dependencies(name).unwrap_or_default()),
                    );
                    obj.insert("kind".to_string(), serde_json::json!(kind));
                    obj.insert("run".to_string(), serde_json::json!(t.run));
                    obj.insert(
                        "cached".to_string(),
                        serde_json::json!(
                            config.settings.cache && Cache::caches(t) && !t.foreground
                        ),
                    );
                    obj.insert("foreground".to_string(), serde_json::json!(t.foreground));
                    obj.insert("sources".to_string(), serde_json::json!(t.sources));
                    obj.insert("outputs".to_string(), serde_json::json!(t.outputs));
                    tasks.insert(name.to_string(), serde_json::Value::Object(obj));
                }
            }
            outln!("{}", serde_json::to_string_pretty(&tasks).unwrap());
        }

        ListFormat::Plain => {
            let mut names: Vec<_> = graph.task_names().collect();
            names.sort_unstable();
            for name in names {
                outln!("{name}");
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
            outln!("{}", style("Task dependency graph:").bold());
            outln!();

            for task_node in &tasks {
                let deps = graph.dependencies(&task_node.name).unwrap_or_default();

                if deps.is_empty() {
                    outln!("  {}", style(&task_node.name).cyan().bold());
                } else {
                    outln!(
                        "  {} {} {}",
                        style(&task_node.name).cyan().bold(),
                        style("←").dim(),
                        deps.join(", ")
                    );
                }
            }
        }

        GraphFormat::Dot => {
            outln!("digraph yatr {{");
            outln!("  rankdir=LR;");
            outln!("  node [shape=box];");

            for task_node in &tasks {
                if let Some(deps) = graph.dependencies(&task_node.name) {
                    for dep in deps {
                        outln!("  \"{}\" -> \"{}\";", dep, task_node.name);
                    }
                }
            }

            outln!("}}");
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

            outln!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
    }

    Ok(())
}

fn init_config(force: bool) -> Result<()> {
    let name = config::CONFIG_FILES
        .iter()
        .copied()
        .find(|n| std::path::Path::new(n).exists())
        .unwrap_or(config::CONFIG_FILES[0]);
    let path = std::path::Path::new(name);

    if path.exists() && !force {
        return Err(YatrError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{name} already exists (use --force to overwrite)"),
        )));
    }

    let template = r#"# yatr.toml - Task runner configuration
# See https://cargopete.github.io/yatr/ for documentation

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
no_cache = true             # rewrites files, so always run it

[tasks.lint]
desc = "Run clippy"
run = ["cargo clippy -- -D warnings"]
sources = ["**/*.rs", "Cargo.*"]

[tasks.test]
desc = "Run tests"
run = ["cargo test"]
sources = ["**/*.rs", "Cargo.*"]

[tasks.ci]
desc = "Format, lint, and test"
depends = ["fmt", "lint", "test"]
run = []

[tasks.build]
desc = "Build release binary"
depends = ["ci"]
run = ["cargo build --release"]
sources = ["**/*.rs", "Cargo.*"]
"#;

    std::fs::write(path, template)?;

    outln!("{} Created {}", style("✓").green(), style(name).bold());

    Ok(())
}
