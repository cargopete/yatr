//! `yatr explain`: whether a task would run or be restored from cache, and
//! which of its inputs changed since it last ran.
//!
//! Nothing is executed. Keys are computed as a run computes them, so the
//! verdict is the one a run would reach, with one exception: a dependency
//! that is not cached and declares `outputs` is fingerprinted by what it
//! leaves on disk, which a run only knows after running it.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cache::{Cache, KeyInputs};
use crate::config::{Config, TaskConfig};
use crate::error::Result;
use crate::graph::TaskGraph;

/// Entries listed under one change before the rest are only counted.
const LISTED: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// A run restores it from the cache
    Cached,
    /// A run executes it: no usable cache entry for its current inputs
    Runs,
    /// A run always executes it: it is never cached
    AlwaysRuns,
    /// Nothing to run of its own, only dependencies
    Aggregate,
}

/// One kind of input that differs from the task's last execution.
#[derive(Debug, Clone, Serialize)]
pub struct Change {
    /// `sources`, `dependencies`, `definition`, `env`, `env_inputs`,
    /// `tool_inputs`, `toolchains` or `outputs`
    pub input: &'static str,
    pub summary: String,
    /// What changed, at most [`LISTED`] of them
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskExplanation {
    pub name: String,
    pub decision: Decision,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// When the cache entry a run would restore was written
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<DateTime<Utc>>,
    /// How long the cached execution took, which a hit saves
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_ms: Option<u64>,
    /// When the task last executed here
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
}

impl TaskExplanation {
    fn new(name: &str, decision: Decision, reason: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            decision,
            reason: reason.into(),
            key: None,
            cached_at: None,
            saved_ms: None,
            last_run: None,
            changes: Vec::new(),
        }
    }
}

const fn is_aggregate(task: &TaskConfig) -> bool {
    task.run.is_empty() && task.script.is_none() && task.wasm.is_none()
}

/// Explain `task` and everything it depends on, the task itself first.
pub fn explain(
    task: &str,
    config: &Config,
    graph: &TaskGraph,
    cache: Option<&Cache>,
    run_cwd: &Path,
) -> Result<Vec<TaskExplanation>> {
    let order = graph.execution_order(task)?;
    let mut fingerprints: HashMap<&str, String> = HashMap::new();
    let mut explained = Vec::with_capacity(order.len());

    for node in order {
        let name = node.name.as_str();
        let task = &node.config;
        let dep_fingerprints = |fingerprints: &HashMap<&str, String>| -> Vec<(String, String)> {
            graph
                .dependencies(name)
                .unwrap_or_default()
                .into_iter()
                .map(|d| {
                    let fingerprint = fingerprints
                        .get(d)
                        .cloned()
                        .unwrap_or_else(|| d.to_string());
                    (d.to_string(), fingerprint)
                })
                .collect()
        };
        if is_aggregate(task) {
            let deps: Vec<String> = dep_fingerprints(&fingerprints)
                .into_iter()
                .map(|(_, fingerprint)| fingerprint)
                .collect();
            fingerprints.insert(name, Cache::aggregate_fingerprint(name, &deps));
            explained.push(TaskExplanation::new(
                name,
                Decision::Aggregate,
                "runs nothing itself, only its dependencies",
            ));
            continue;
        }
        let Some(cache) = cache else {
            explained.push(TaskExplanation::new(
                name,
                Decision::AlwaysRuns,
                "caching is off (`[settings] cache = false`)",
            ));
            continue;
        };

        let cwd = task.working_dir().unwrap_or_else(|| run_cwd.to_path_buf());
        let deps = dep_fingerprints(&fingerprints);
        let inputs = Cache::key_inputs(name, task, config, &deps, &cwd)?;
        fingerprints.insert(name, Cache::fingerprint(&inputs.key, task, &cwd));

        let mut e = if task.no_cache {
            TaskExplanation::new(name, Decision::AlwaysRuns, "`no_cache = true`")
        } else if task.sources.is_empty() {
            TaskExplanation::new(
                name,
                Decision::AlwaysRuns,
                "it declares no `sources`, so there is nothing to tell a stale result from a fresh one",
            )
        } else if task.foreground {
            TaskExplanation::new(
                name,
                Decision::AlwaysRuns,
                "foreground tasks are never cached",
            )
        } else {
            lookup(name, &inputs, cache, &cwd)
        };
        e.key = Some(inputs.key);
        explained.push(e);
    }

    explained.reverse();
    Ok(explained)
}

/// The verdict for a cacheable task: a hit, or a miss and what brought it about.
fn lookup(name: &str, inputs: &KeyInputs, cache: &Cache, cwd: &Path) -> TaskExplanation {
    if let Some(entry) = cache.peek(name, &inputs.key, cwd) {
        let mut e = TaskExplanation::new(
            name,
            Decision::Cached,
            "its inputs match a cache entry, which a run restores",
        );
        e.cached_at = Some(entry.created_at);
        e.saved_ms = Some(entry.duration_ms);
        return e;
    }

    let remote = if cache.reads_remote() {
        "; the remote cache is only consulted by a run"
    } else {
        ""
    };
    let Some(last) = cache.last_run(name, cwd) else {
        return TaskExplanation::new(
            name,
            Decision::Runs,
            format!("there is no record of it running here before{remote}"),
        );
    };

    let changes = diff(&last.inputs, inputs);
    let reason = if last.inputs.key == inputs.key {
        if last.success {
            "its inputs are unchanged, but the cache entry is gone or incomplete".to_string()
        } else {
            "its last run, with these same inputs, failed".to_string()
        }
    } else if changes.is_empty() {
        "its inputs look unchanged, but the last run was keyed by a different yatr".to_string()
    } else {
        format!("its inputs changed since it last ran{remote}")
    };
    let mut e = TaskExplanation::new(name, Decision::Runs, reason);
    e.last_run = Some(last.at);
    e.changes = changes;
    e
}

/// Compare two digest maps, labelling entries only in `new`, only in `old`,
/// and in both with different digests.
fn map_changes(
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
    [added, removed, changed]: [&str; 3],
) -> (Vec<String>, [usize; 3]) {
    let mut items = Vec::new();
    let mut counts = [0; 3];
    for (name, digest) in new {
        match old.get(name) {
            None => {
                counts[0] += 1;
                items.push(format!("{added} {name}"));
            }
            Some(before) if before != digest => {
                counts[2] += 1;
                items.push(format!("{changed} {name}"));
            }
            Some(_) => {}
        }
    }
    for name in old.keys().filter(|name| !new.contains_key(*name)) {
        counts[1] += 1;
        items.push(format!("{removed} {name}"));
    }
    (items, counts)
}

fn change(
    input: &'static str,
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
    labels: [&str; 3],
) -> Option<Change> {
    let (mut items, counts) = map_changes(old, new, labels);
    if items.is_empty() {
        return None;
    }
    let summary = [
        (counts[2], labels[2]),
        (counts[0], labels[0]),
        (counts[1], labels[1]),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>()
    .join(", ");
    items.truncate(LISTED);
    Some(Change {
        input,
        summary,
        items,
    })
}

/// Every kind of input that differs between a task's last execution and now.
#[must_use]
pub fn diff(old: &KeyInputs, new: &KeyInputs) -> Vec<Change> {
    let mut changes = Vec::new();
    let labels = ["added", "removed", "changed"];

    if old.definition != new.definition {
        changes.push(Change {
            input: "definition",
            summary: "its commands, script, plugin, `cwd` or shell mode changed".to_string(),
            items: Vec::new(),
        });
    }
    changes.extend(change(
        "sources",
        &old.sources,
        &new.sources,
        ["added", "removed", "modified"],
    ));
    changes.extend(change("dependencies", &old.deps, &new.deps, labels));
    changes.extend(change("env", &old.env, &new.env, labels));
    changes.extend(change(
        "env_inputs",
        &old.env_inputs,
        &new.env_inputs,
        labels,
    ));
    changes.extend(change(
        "tool_inputs",
        &old.tool_inputs,
        &new.tool_inputs,
        labels,
    ));
    changes.extend(change(
        "toolchains",
        &old.toolchains,
        &new.toolchains,
        labels,
    ));
    if old.outputs != new.outputs {
        changes.push(Change {
            input: "outputs",
            summary: "its declared `outputs` patterns changed".to_string(),
            items: Vec::new(),
        });
    }
    changes
}

fn duration(ms: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let secs = ms as f64 / 1000.0;
    format!("{secs:.2}s")
}

/// The explanation as text, one task per paragraph.
#[must_use]
pub fn render(explained: &[TaskExplanation]) -> String {
    let mut out = String::new();
    for e in explained {
        let verdict = match e.decision {
            Decision::Cached => "cached",
            Decision::Runs => "runs",
            Decision::AlwaysRuns => "always runs",
            Decision::Aggregate => "aggregate",
        };
        let _ = write!(out, "{}: {verdict} - {}", e.name, e.reason);
        if let (Some(at), Some(ms)) = (e.cached_at, e.saved_ms) {
            let _ = write!(
                out,
                " (written {}, saves {})",
                at.format("%Y-%m-%d %H:%M UTC"),
                duration(ms)
            );
        }
        if let Some(at) = e.last_run {
            let _ = write!(out, " (last ran {})", at.format("%Y-%m-%d %H:%M UTC"));
        }
        out.push('\n');
        for c in &e.changes {
            let _ = writeln!(out, "  {}: {}", c.input, c.summary);
            for item in &c.items {
                let _ = writeln!(out, "    {item}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(sources: &[(&str, &str)]) -> KeyInputs {
        KeyInputs {
            sources: sources
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..KeyInputs::default()
        }
    }

    #[test]
    fn diff_names_added_removed_and_modified_sources() {
        let old = inputs(&[("a.rs", "1"), ("b.rs", "1"), ("c.rs", "1")]);
        let new = inputs(&[("a.rs", "2"), ("b.rs", "1"), ("d.rs", "1")]);
        let changes = diff(&old, &new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].input, "sources");
        assert_eq!(changes[0].summary, "1 modified, 1 added, 1 removed");
        assert_eq!(
            changes[0].items,
            ["modified a.rs", "added d.rs", "removed c.rs"]
        );
    }

    #[test]
    fn diff_of_equal_inputs_is_empty() {
        let same = inputs(&[("a.rs", "1")]);
        assert!(diff(&same, &same).is_empty());
    }
}
