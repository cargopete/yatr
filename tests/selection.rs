//! Task selection from the command line: shorthand names, run flags, and
//! several tasks in one invocation.

use std::path::Path;

use assert_cmd::Command;

fn yatr(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

const DEPLOY: &str = "[tasks.deploy]\nrun = [\"touch DEPLOYED\"]\nno_cache = true\n";

/// Run flags after a shorthand task name are flags, so a dry run never executes.
#[test]
fn flags_after_shorthand_task_apply_to_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), DEPLOY).unwrap();

    let out = yatr(dir.path(), &["deploy", "--dry-run"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.path().join("DEPLOYED").exists(), "dry run executed");
    assert!(String::from_utf8_lossy(&out.stdout).contains("deploy"));
}

#[test]
fn run_flags_before_shorthand_task_apply_to_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), DEPLOY).unwrap();

    let out = yatr(dir.path(), &["--dry-run", "deploy"]);
    assert!(out.status.success());
    assert!(!dir.path().join("DEPLOYED").exists(), "dry run executed");
}

/// A top-level run flag in front of a subcommand would be silently ignored.
#[test]
fn run_flags_before_a_subcommand_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), DEPLOY).unwrap();

    let out = yatr(dir.path(), &["--dry-run", "run", "deploy"]);
    assert!(!out.status.success());
    assert!(!dir.path().join("DEPLOYED").exists());
}

#[test]
fn unknown_flag_after_shorthand_task_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), DEPLOY).unwrap();

    let out = yatr(dir.path(), &["deploy", "--no-such-flag"]);
    assert_eq!(out.status.code(), Some(2), "expected a clap usage error");
    assert!(!dir.path().join("DEPLOYED").exists());
}

/// Every requested name is validated before anything runs.
#[test]
fn unknown_task_after_a_known_one_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), DEPLOY).unwrap();

    for args in [&["deploy", "nosuch"][..], &["run", "deploy", "nosuch"]] {
        let out = yatr(dir.path(), args);
        assert!(!out.status.success(), "{args:?} succeeded");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("nosuch"),
            "error should name the missing task"
        );
        assert!(
            !dir.path().join("DEPLOYED").exists(),
            "{args:?} ran deploy before failing"
        );
    }
}

const SHARED_DEP: &str = "\
[tasks.setup]
shell = true
run = [\"echo setup-ran >> log.txt\"]
no_cache = true
[tasks.a]
depends = [\"setup\"]
run = [\"true\"]
no_cache = true
[tasks.b]
depends = [\"setup\"]
run = [\"true\"]
no_cache = true
";

/// Several named tasks form one graph: a shared dependency runs once.
#[test]
fn several_tasks_share_one_graph() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), SHARED_DEP).unwrap();

    for args in [&["run", "a", "b"][..], &["a", "b"]] {
        let _ = std::fs::remove_file(dir.path().join("log.txt"));
        let out = yatr(dir.path(), args);
        assert!(out.status.success());
        let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
        assert_eq!(log.lines().count(), 1, "{args:?}: setup ran {log:?}");

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            stderr.matches("tasks completed").count(),
            1,
            "{args:?}: expected one summary: {stderr}"
        );
    }
}

#[test]
fn several_tasks_dry_run_prints_one_plan() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), SHARED_DEP).unwrap();

    let out = yatr(dir.path(), &["run", "--dry-run", "a", "b"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("Execution plan").count(),
        1,
        "plan: {stdout}"
    );
    assert!(!dir.path().join("log.txt").exists());
}
