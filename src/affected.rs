//! Git-based "what's affected" detection.
//!
//! Given a git ref, figures out which tasks could be affected by the changes
//! since that ref: a task is *directly* affected when one of its `sources` or
//! `watch` globs matches a changed file, and a task with no declared inputs is
//! treated as always affected (we can't prove otherwise). The affected set is
//! then propagated to every transitive *dependent* — if a task's inputs change,
//! everything downstream of it must be reconsidered too.
//!
//! Caching already gives correctness (unchanged tasks are cache hits); this is
//! about *speed at scale* — not even looking at tasks git says can't have moved.

use std::collections::HashSet;
use std::process::Command;

use globset::{Glob, GlobSetBuilder};

use crate::error::{Result, YatrError};
use crate::graph::TaskGraph;

/// List files changed since `git_ref`, relative to the current directory.
pub fn changed_files(git_ref: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--relative", git_ref])
        .output()
        .map_err(|e| YatrError::Cache {
            message: format!("failed to run git: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(YatrError::Cache {
            message: format!("git diff failed: {}", stderr.trim()),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect())
}

/// Compute the set of task names affected by the given changed files.
#[must_use]
pub fn affected_tasks(graph: &TaskGraph, changed: &[String]) -> HashSet<String> {
    let mut affected: HashSet<String> = HashSet::new();

    // Directly affected: a task whose declared inputs match a changed file, or
    // a task that declares no inputs (conservatively assumed affected).
    for name in graph.task_names() {
        let Some(task) = graph.get_task(name) else {
            continue;
        };
        let patterns: Vec<&String> = task
            .config
            .sources
            .iter()
            .chain(task.config.watch.iter())
            .collect();

        let is_affected = if patterns.is_empty() {
            true
        } else {
            let mut builder = GlobSetBuilder::new();
            for p in patterns {
                if let Ok(g) = Glob::new(p) {
                    builder.add(g);
                }
            }
            builder
                .build()
                .ok()
                .is_some_and(|set| changed.iter().any(|f| set.is_match(f)))
        };

        if is_affected {
            affected.insert(name.to_string());
        }
    }

    // Propagate to transitive dependents: downstream of a changed task is also
    // affected.
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
        let aff = affected_tasks(&g, &["web/index.html".to_string()]);
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
        let aff = affected_tasks(&g, &["lib/core.rs".to_string()]);
        assert!(aff.contains("lib"));
        assert!(aff.contains("app"));
    }

    #[test]
    fn no_sources_is_always_affected() {
        let g = graph(
            "[tasks.always]\nrun=[\"echo hi\"]\n\
             [tasks.scoped]\nsources=[\"src/**\"]\nrun=[\"echo scoped\"]\n",
        );
        let aff = affected_tasks(&g, &["docs/readme.md".to_string()]);
        assert!(aff.contains("always")); // no declared inputs → always affected
        assert!(!aff.contains("scoped")); // sources don't match
    }
}
