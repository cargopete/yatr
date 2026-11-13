//! Git-based "what's affected" detection.
//!
//! Given a git ref, figures out which tasks could be affected by the changes
//! since that ref's merge-base with `HEAD`, including uncommitted and untracked
//! files. A task is *directly* affected when one of its `sources` or `watch`
//! globs (relative to its `cwd`) matches a changed file, when its definition in
//! the config changed, or when it runs something but declares no inputs (we
//! can't prove otherwise). The affected set is then propagated to every
//! transitive *dependent*, so an aggregate with no commands of its own is
//! affected exactly when one of its dependencies is.
//!
//! Caching already gives correctness (unchanged tasks are cache hits); this is
//! about *speed at scale* — not even looking at tasks git says can't have moved.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use globset::GlobSetBuilder;

use crate::config::{Config, TaskConfig};
use crate::error::{Result, YatrError};
use crate::globs;
use crate::graph::TaskGraph;

/// The outcome of affected detection.
#[derive(Debug)]
pub struct Affected {
    /// The commit the working tree was compared against.
    pub merge_base: String,
    /// Changed, deleted and untracked files, relative to the current directory.
    pub changed: Vec<String>,
    /// Affected task names.
    pub tasks: HashSet<String>,
}

/// Work out what is affected by the changes since `git_ref`.
pub fn detect(
    git_ref: &str,
    config: &Config,
    config_path: &Path,
    graph: &TaskGraph,
) -> Result<Affected> {
    let merge_base = git(&["merge-base", git_ref, "HEAD"])
        .map_err(|e| {
            affected_err(format!(
                "could not determine changes since '{git_ref}': {e}"
            ))
        })?
        .trim()
        .to_string();
    let changed = changed_files(&merge_base)?;
    let redefined = redefined_tasks(config, config_path, &merge_base);
    let root = std::env::current_dir()?;
    let root = root.canonicalize().unwrap_or(root);
    let tasks = affected_tasks(graph, &changed, &root, &redefined);
    Ok(Affected {
        merge_base,
        changed,
        tasks,
    })
}

const fn affected_err(message: String) -> YatrError {
    YatrError::Affected { message }
}

/// Run git, returning stdout or the first line of stderr.
fn git(args: &[&str]) -> std::result::Result<String, String> {
    git_in(None, args)
}

fn git_in(dir: Option<&Path>, args: &[&str]) -> std::result::Result<String, String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    let output = cmd
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git (is it installed?): {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr
            .lines()
            .next()
            .unwrap_or("git failed")
            .trim()
            .to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Files that differ between `base` and the working tree, plus untracked
/// files, relative to the current directory.
pub fn changed_files(base: &str) -> Result<Vec<String>> {
    let err = |e: String| affected_err(format!("could not list changes since {base}: {e}"));
    let tracked = git(&["diff", "--name-only", "--relative", base]).map_err(err)?;
    let untracked = git(&["ls-files", "--others", "--exclude-standard"]).map_err(err)?;

    let mut files: Vec<String> = tracked
        .lines()
        .chain(untracked.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // yatr's own cache dir, matching what the cache never hashes.
        .filter(|l| !Path::new(l).components().any(|c| c.as_os_str() == ".yatr"))
        .map(ToString::to_string)
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Tasks whose definition differs between the config at `base` and now. If
/// the old config cannot be read (it did not exist, say), every task counts.
fn redefined_tasks(config: &Config, config_path: &Path, base: &str) -> HashSet<String> {
    let read = |path: &Path| -> std::io::Result<String> {
        let dir = path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file = path.file_name().unwrap_or_default().to_string_lossy();
        git_in(Some(dir), &["show", &format!("{base}:./{file}")]).map_err(std::io::Error::other)
    };
    Config::load_via(config_path, &read).map_or_else(
        |_| config.tasks.keys().cloned().collect(),
        |old| changed_task_definitions(&old, config),
    )
}

/// Names of tasks in `new` that are absent from `old` or would run differently.
#[must_use]
pub fn changed_task_definitions(old: &Config, new: &Config) -> HashSet<String> {
    let toolchains_changed =
        serde_json::to_value(&old.toolchain).ok() != serde_json::to_value(&new.toolchain).ok();

    new.tasks
        .iter()
        .filter(|(name, task)| {
            toolchains_changed
                || old.tasks.get(*name).is_none_or(|prev| {
                    behaviour(prev) != behaviour(task) || old.task_env(prev) != new.task_env(task)
                })
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// A task's definition minus what cannot change what it does.
fn behaviour(task: &TaskConfig) -> Option<serde_json::Value> {
    let mut task = task.clone();
    task.desc = None;
    serde_json::to_value(task).ok()
}

/// Where a task's patterns resolve: its `cwd` under `root`, lexically
/// normalised, or `None` if that leaves `root` (git paths cannot reach it).
fn task_dir(task: &TaskConfig, root: &Path) -> Option<PathBuf> {
    let joined = task
        .working_dir()
        .map_or_else(|| root.to_path_buf(), |w| root.join(w));
    let mut dir = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                dir.pop();
            }
            other => dir.push(other),
        }
    }
    dir.starts_with(root).then_some(dir)
}

const fn has_commands(task: &TaskConfig) -> bool {
    !task.run.is_empty() || task.script.is_some() || task.wasm.is_some()
}

fn matches_inputs(task: &TaskConfig, changed: &[String], root: &Path) -> bool {
    let patterns: Vec<&String> = task.sources.iter().chain(task.watch.iter()).collect();
    if patterns.is_empty() {
        return has_commands(task);
    }
    let Some(dir) = task_dir(task, root) else {
        return true;
    };

    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = globs::compile(p) {
            builder.add(g);
        }
    }
    let Ok(set) = builder.build() else {
        return true;
    };
    changed
        .iter()
        .filter_map(|f| root.join(f).strip_prefix(&dir).ok().map(Path::to_path_buf))
        .any(|rel| set.is_match(rel))
}

/// Compute the set of task names affected by the given changed files, which
/// are relative to `root`, with `redefined` tasks counted as directly affected.
#[must_use]
pub fn affected_tasks<S: BuildHasher>(
    graph: &TaskGraph,
    changed: &[String],
    root: &Path,
    redefined: &HashSet<String, S>,
) -> HashSet<String> {
    let mut affected: HashSet<String> = graph
        .task_names()
        .filter(|name| {
            redefined.contains(*name)
                || graph
                    .get_task(name)
                    .is_some_and(|t| matches_inputs(&t.config, changed, root))
        })
        .map(ToString::to_string)
        .collect();

    let mut stack: Vec<String> = affected.iter().cloned().collect();
    while let Some(name) = stack.pop() {
        if let Some(dependents) = graph.dependents(&name) {
            for dep in dependents {
                if affected.insert(dep.to_string()) {
                    stack.push(dep.to_string());
                }
            }
        }
    }

    affected
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/repo";

    fn graph(toml: &str) -> TaskGraph {
        let config: crate::config::Config = toml::from_str(toml).unwrap();
        TaskGraph::from_config(&config).unwrap()
    }

    #[test]
    fn directly_affected_by_source_match() {
        let g = graph(
            "[tasks.frontend]\nsources=[\"web/**\"]\nrun=[\"echo fe\"]\n\
             [tasks.backend]\nsources=[\"api/**\"]\nrun=[\"echo be\"]\n",
        );
        let aff = affected_tasks(
            &g,
            &["web/index.html".to_string()],
            Path::new(ROOT),
            &HashSet::new(),
        );
        assert!(aff.contains("frontend"));
        assert!(!aff.contains("backend"));
    }

    #[test]
    fn propagates_to_dependents() {
        let g = graph(
            "[tasks.lib]\nsources=[\"lib/**\"]\nrun=[\"echo lib\"]\n\
             [tasks.app]\ndepends=[\"lib\"]\nsources=[\"app/**\"]\nrun=[\"echo app\"]\n",
        );
        // A change in lib affects app, which depends on it.
        let aff = affected_tasks(
            &g,
            &["lib/core.rs".to_string()],
            Path::new(ROOT),
            &HashSet::new(),
        );
        assert!(aff.contains("lib"));
        assert!(aff.contains("app"));
    }

    #[test]
    fn no_sources_is_always_affected() {
        let g = graph(
            "[tasks.always]\nrun=[\"echo hi\"]\n\
             [tasks.scoped]\nsources=[\"src/**\"]\nrun=[\"echo scoped\"]\n",
        );
        let aff = affected_tasks(
            &g,
            &["docs/readme.md".to_string()],
            Path::new(ROOT),
            &HashSet::new(),
        );
        assert!(aff.contains("always")); // no declared inputs → always affected
        assert!(!aff.contains("scoped")); // sources don't match
    }

    #[test]
    fn sources_resolve_against_task_cwd() {
        let g = graph("[tasks.site]\ncwd=\"web\"\nsources=[\"src/**\"]\nrun=[\"echo\"]\n");
        let changed =
            |f: &str| affected_tasks(&g, &[f.to_string()], Path::new(ROOT), &HashSet::new());
        assert!(changed("web/src/a.ts").contains("site"));
        assert!(!changed("src/a.ts").contains("site"));
    }

    #[test]
    fn cwd_outside_the_root_is_always_affected() {
        let g = graph("[tasks.t]\ncwd=\"../elsewhere\"\nsources=[\"src/**\"]\nrun=[\"echo\"]\n");
        let aff = affected_tasks(&g, &[], Path::new(ROOT), &HashSet::new());
        assert!(aff.contains("t"));
    }

    #[test]
    fn star_does_not_cross_directories() {
        let g = graph("[tasks.t]\nsources=[\"src/*.rs\"]\nrun=[\"echo\"]\n");
        let aff = affected_tasks(
            &g,
            &["src/nested/a.rs".to_string()],
            Path::new(ROOT),
            &HashSet::new(),
        );
        assert!(!aff.contains("t"));
    }

    #[test]
    fn aggregate_is_affected_only_through_dependencies() {
        let g = graph(
            "[tasks.a]\nsources=[\"a/**\"]\nrun=[\"echo a\"]\n\
             [tasks.b]\nsources=[\"b/**\"]\nrun=[\"echo b\"]\n\
             [tasks.ci]\ndepends=[\"a\",\"b\"]\nrun=[]\n",
        );
        let none = affected_tasks(
            &g,
            &["docs/x.md".to_string()],
            Path::new(ROOT),
            &HashSet::new(),
        );
        assert!(none.is_empty(), "{none:?}");
        let some = affected_tasks(&g, &["a/x".to_string()], Path::new(ROOT), &HashSet::new());
        assert!(some.contains("ci") && !some.contains("b"));
    }

    #[test]
    fn redefined_tasks_are_affected_and_propagate() {
        let g = graph(
            "[tasks.lib]\nsources=[\"lib/**\"]\nrun=[\"echo lib\"]\n\
             [tasks.app]\ndepends=[\"lib\"]\nsources=[\"app/**\"]\nrun=[\"echo app\"]\n",
        );
        let redefined: HashSet<String> = ["lib".to_string()].into();
        let aff = affected_tasks(&g, &[], Path::new(ROOT), &redefined);
        assert!(aff.contains("lib") && aff.contains("app"));
    }

    #[test]
    fn config_diff_ignores_desc_and_sees_run_and_env() {
        let parse = |s: &str| -> Config { toml::from_str(s).unwrap() };
        let old = parse(
            "[env]\nX=\"1\"\n[tasks.a]\nrun=[\"echo a\"]\n[tasks.b]\nrun=[\"echo b\"]\n\
             [tasks.c]\nrun=[\"echo c\"]\n",
        );
        let new = parse(
            "[env]\nX=\"1\"\n[tasks.a]\ndesc=\"now documented\"\nrun=[\"echo a\"]\n\
             [tasks.b]\nrun=[\"echo B\"]\n[tasks.c]\nrun=[\"echo c\"]\n[tasks.d]\nrun=[\"echo d\"]\n",
        );
        let mut got: Vec<String> = changed_task_definitions(&old, &new).into_iter().collect();
        got.sort();
        assert_eq!(got, ["b", "d"]);

        let env_changed = parse(&format!(
            "[env]\nX=\"2\"\n{}",
            "[tasks.a]\nrun=[\"echo a\"]\n"
        ));
        assert!(changed_task_definitions(&old, &env_changed).contains("a"));
    }
}
