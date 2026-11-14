//! Reports of a run for readers that are not a person at a terminal: the
//! `--json` document, the `--ndjson` event stream and the `--agent` text.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::YatrError;
use crate::executor::TaskResult;
use crate::output::outln;

/// How a run reports itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Human,
    Json,
    Ndjson,
    Agent,
}

/// Lines of a failing task's output shown by `--agent` unless `--tail` says otherwise.
pub const AGENT_TAIL: usize = 40;

/// Longest output line `--agent` prints before clipping it.
const AGENT_LINE: usize = 400;

static EMITTED: AtomicBool = AtomicBool::new(false);

/// Whether the run's closing document or event has been written, in which
/// case an error is already part of it and must not be reported twice.
pub fn emitted() -> bool {
    EMITTED.load(Ordering::Relaxed)
}

/// The last lines of a task's output.
pub struct Tail<'a> {
    pub text: &'a str,
    pub total: usize,
    pub shown: usize,
}

/// The last `lines` lines of `output`, which is returned whole if it has no more.
#[must_use]
pub fn tail(output: &str, lines: usize) -> Tail<'_> {
    let body = output.trim_end_matches(['\n', '\r']);
    let total = if body.is_empty() {
        0
    } else {
        body.lines().count()
    };
    if total <= lines {
        return Tail {
            text: output,
            total,
            shown: total,
        };
    }
    let text = lines.checked_sub(1).map_or("", |nth| {
        body.rmatch_indices('\n')
            .nth(nth)
            .map_or(body, |(at, _)| &body[at + 1..])
    });
    Tail {
        text,
        total,
        shown: lines,
    }
}

/// Write a task's whole output to `<dir>/<task>.log`, returning the path
/// relative to `cwd` where it can be. The directory ignores itself, so it
/// never shows up in `git status`.
pub fn write_log(dir: &Path, cwd: &Path, result: &TaskResult) -> Option<PathBuf> {
    let output = result.output.as_deref()?;
    std::fs::create_dir_all(dir).ok()?;
    if let Some(parent) = dir.parent() {
        let ignore = parent.join(".gitignore");
        if !ignore.exists() {
            let _ = std::fs::write(ignore, "*\n");
        }
    }
    let name: String = result
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{name}.log"));
    std::fs::write(&path, output).ok()?;
    Some(
        path.strip_prefix(cwd)
            .map(Path::to_path_buf)
            .unwrap_or(path),
    )
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One task's entry in the `--json` document, and the body of its
/// `task_finish` event.
#[must_use]
pub fn task_json(r: &TaskResult, tail_lines: Option<usize>) -> Value {
    let mut obj = json!({
        "name": r.name,
        "success": r.success,
        "cached": r.cached,
        "cache": r.cache.as_str(),
        "duration_ms": ms(r.duration),
        "output": r.output,
        "error": r.error,
        "command": r.command,
        "exit_code": r.exit_code,
        "cancelled": r.cancelled,
        "allowed_failure": r.allowed_failure,
    });
    if let (Some(lines), Some(output)) = (tail_lines, &r.output) {
        let t = tail(output, lines);
        obj["output"] = json!(t.text);
        obj["output_lines"] = json!(t.total);
        obj["output_truncated"] = json!(t.shown < t.total);
    }
    if let Some(log) = &r.log {
        obj["log"] = json!(log);
    }
    obj
}

fn summary_json(results: &[TaskResult]) -> Value {
    let first = results.iter().map(|r| ms(r.start_offset)).min();
    let last = results
        .iter()
        .map(|r| ms(r.start_offset + r.duration))
        .max();
    json!({
        "succeeded": results.iter().filter(|r| r.success).count(),
        "failed": results.iter().filter(|r| !r.success && !r.cancelled).count(),
        "cancelled": results.iter().filter(|r| r.cancelled).count(),
        "cached": results.iter().filter(|r| r.cached).count(),
        "duration_ms": last.unwrap_or_default().saturating_sub(first.unwrap_or_default()),
    })
}

fn print(value: &Value, pretty: bool) {
    let text = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    outln!("{}", text.unwrap_or_default());
}

/// Print the `--json` document that closes a run.
pub fn run_document(results: &[TaskResult], tail_lines: Option<usize>, error: Option<&YatrError>) {
    let tasks: Vec<Value> = results.iter().map(|r| task_json(r, tail_lines)).collect();
    let mut doc = json!({
        "ok": error.is_none(),
        "tasks": tasks,
        "summary": summary_json(results),
    });
    if let Some(e) = error {
        doc["error"] = e.to_json();
    }
    EMITTED.store(true, Ordering::Relaxed);
    print(&doc, true);
}

/// Print an error as the whole of a command's JSON output.
pub fn error_document(error: &YatrError, mode: Mode) {
    match mode {
        Mode::Ndjson => print(
            &json!({ "event": "error", "error": error.to_json() }),
            false,
        ),
        _ => print(&json!({ "error": error.to_json() }), true),
    }
}

/// `--ndjson`: the run is about to start `tasks`, in this order at the latest.
pub fn event_run_start(tasks: &[&str]) {
    print(&json!({ "event": "run_start", "tasks": tasks }), false);
}

pub fn event_task_start(task: &str) {
    print(&json!({ "event": "task_start", "task": task }), false);
}

pub fn event_output(task: &str, line: &str, to_stderr: bool) {
    let stream = if to_stderr { "stderr" } else { "stdout" };
    print(
        &json!({ "event": "output", "task": task, "stream": stream, "line": line }),
        false,
    );
}

pub fn event_task_finish(r: &TaskResult, tail_lines: Option<usize>) {
    let mut event = task_json(r, tail_lines);
    event["event"] = json!("task_finish");
    print(&event, false);
}

pub fn event_run_finish(results: &[TaskResult], error: Option<&YatrError>) {
    let mut event = json!({
        "event": "run_finish",
        "ok": error.is_none(),
        "summary": summary_json(results),
    });
    if let Some(e) = error {
        event["error"] = e.to_json();
    }
    EMITTED.store(true, Ordering::Relaxed);
    print(&event, false);
}

fn seconds(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// `--agent`: one line for a finished task, and for a failure the tail of
/// what it printed.
pub fn agent_task(r: &TaskResult, tail_lines: usize) {
    if r.cancelled {
        outln!("skip {} (cancelled)", r.name);
        return;
    }
    if r.success {
        if r.cached {
            outln!("ok   {} (cached)", r.name);
        } else {
            outln!("ok   {} {}", r.name, seconds(r.duration));
        }
        return;
    }

    let what = match (&r.command, r.exit_code) {
        (Some(command), Some(code)) => format!("exit {code} from `{command}`"),
        _ => r.error.clone().unwrap_or_else(|| "failed".to_string()),
    };
    if r.allowed_failure {
        outln!(
            "warn {} {}: {what} (allow_failure)",
            r.name,
            seconds(r.duration)
        );
        if let Some(log) = &r.log {
            outln!("     log: {}", log.display());
        }
        return;
    }
    outln!("FAIL {} {}: {what}", r.name, seconds(r.duration));

    let output = r.output.as_deref().unwrap_or_default();
    let t = tail(output, tail_lines);
    if t.total == 0 {
        return;
    }
    let log = r
        .log
        .as_ref()
        .map(|p| format!(", full log: {}", p.display()))
        .unwrap_or_default();
    if t.shown < t.total {
        outln!(
            "--- {}: last {} of {} lines{log} ---",
            r.name,
            t.shown,
            t.total
        );
    } else {
        outln!("--- {}: {} lines{log} ---", r.name, t.total);
    }
    for line in t.text.lines() {
        match line.char_indices().nth(AGENT_LINE) {
            Some((at, _)) => outln!("{} [+{} chars]", &line[..at], line[at..].chars().count()),
            None => outln!("{line}"),
        }
    }
    outln!("--- end {} ---", r.name);
}

/// `--agent`: the line that closes a run, and how to re-run what failed.
pub fn agent_summary(results: &[TaskResult], planned: usize, wall: Duration) {
    let succeeded = results.iter().filter(|r| r.success).count();
    let cancelled = results.iter().filter(|r| r.cancelled).count();
    let cached = results.iter().filter(|r| r.cached).count();
    let allowed = results.iter().filter(|r| r.allowed_failure).count();
    let failed: Vec<&str> = results
        .iter()
        .filter(|r| !r.success && !r.cancelled && !r.allowed_failure)
        .map(|r| r.name.as_str())
        .collect();
    let not_run = planned.saturating_sub(results.len());

    let mut parts = Vec::new();
    if !failed.is_empty() {
        parts.push(format!("{} failed", failed.len()));
    }
    // Cached tasks are among the ones that passed, not beside them.
    parts.push(if cached > 0 {
        format!("{succeeded} ok ({cached} cached)")
    } else {
        format!("{succeeded} ok")
    });
    for (count, label) in [
        (allowed, "allowed to fail"),
        (cancelled, "cancelled"),
        (not_run, "not run"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    let verdict = if failed.is_empty() && cancelled + not_run == 0 {
        "OK"
    } else {
        "FAILED"
    };
    outln!("{verdict}: {}, {}", parts.join(", "), seconds(wall));
    if !failed.is_empty() {
        outln!("rerun: yatr run --agent {}", failed.join(" "));
    }
}

#[cfg(test)]
mod tests {
    use super::tail;

    #[test]
    fn tail_keeps_the_last_lines() {
        let t = tail("a\nb\nc\nd\n", 2);
        assert_eq!((t.text, t.total, t.shown), ("c\nd", 4, 2));
    }

    #[test]
    fn tail_returns_short_output_whole() {
        let t = tail("a\nb\n", 5);
        assert_eq!((t.text, t.total, t.shown), ("a\nb\n", 2, 2));
        let t = tail("", 5);
        assert_eq!((t.text, t.total, t.shown), ("", 0, 0));
    }

    #[test]
    fn tail_of_zero_lines_is_empty() {
        let t = tail("a\nb\n", 0);
        assert_eq!((t.text, t.total, t.shown), ("", 2, 0));
    }
}
