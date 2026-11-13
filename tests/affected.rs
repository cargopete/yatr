//! End-to-end tests for `yatr affected` and `yatr run --affected` in a real git repo.

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(status.status.success(), "git {args:?}: {status:?}");
}

fn write(dir: &Path, file: &str, content: &str) {
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A repo on branch `main` with one commit holding `config` and `files`.
fn repo(config: &str, files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    write(dir.path(), "yatr.toml", config);
    for f in files {
        write(dir.path(), f, "0");
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

fn affected(dir: &Path, git_ref: &str) -> Vec<String> {
    let out = assert_cmd::Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir)
        .args(["affected", git_ref, "--format", "plain"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(ToString::to_string)
        .collect()
}

const TWO: &str = "[tasks.web]\nsources = [\"web/**\"]\nrun = [\"echo web\"]\n\
                   [tasks.api]\nsources = [\"api/**\"]\nrun = [\"echo api\"]\n";

/// Regression (#21): a new untracked file was not seen.
#[test]
fn untracked_files_are_changes() {
    let dir = repo(TWO, &["web/a", "api/a"]);
    write(dir.path(), "web/new.ts", "x");
    assert_eq!(affected(dir.path(), "HEAD"), ["web"]);
}

/// Regression (#21): editing a task's `run` in yatr.toml affects that task only.
#[test]
fn config_edit_affects_the_changed_task() {
    let dir = repo(TWO, &["web/a", "api/a"]);
    write(
        dir.path(),
        "yatr.toml",
        &TWO.replace("echo api", "echo API"),
    );
    assert_eq!(affected(dir.path(), "HEAD"), ["api"]);
}

/// Regression (#21): `sources` resolve against the task's `cwd`.
#[test]
fn sources_resolve_against_task_cwd() {
    let dir = repo(
        "[tasks.site]\ncwd = \"web\"\nsources = [\"src/**\"]\nrun = [\"echo site\"]\n",
        &["web/src/a.ts"],
    );
    write(dir.path(), "web/src/a.ts", "1");
    assert_eq!(affected(dir.path(), "HEAD"), ["site"]);
}

/// Regression (#21): changes on the ref's side after the fork are not ours.
#[test]
fn diffs_against_the_merge_base() {
    let dir = repo(TWO, &["web/a", "api/a"]);
    git(dir.path(), &["checkout", "-qb", "feature"]);
    write(dir.path(), "web/a", "1");
    git(dir.path(), &["commit", "-qam", "web"]);
    git(dir.path(), &["checkout", "-q", "main"]);
    write(dir.path(), "api/a", "1");
    git(dir.path(), &["commit", "-qam", "api"]);
    git(dir.path(), &["checkout", "-q", "feature"]);
    assert_eq!(affected(dir.path(), "main"), ["web"]);
}

/// Regression (#21): an aggregate is affected only through its dependencies,
/// and `run --affected` skips the unaffected ones.
#[test]
fn run_affected_skips_unaffected_dependencies() {
    let config = format!("{TWO}[tasks.ci]\ndepends = [\"web\", \"api\"]\nrun = []\n");
    let dir = repo(&config, &["web/a", "api/a"]);
    assert!(affected(dir.path(), "HEAD").is_empty());

    write(dir.path(), "web/a", "1");
    let out = assert_cmd::Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "--json", "--affected", "HEAD", "ci"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mut ran: Vec<&str> = json["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    ran.sort_unstable();
    assert_eq!(ran, ["ci", "web"]);
}
