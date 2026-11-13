//! Task paths resolve against the directory of the file that declares the task,
//! not the directory yatr was started from.

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

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const ROOT: &str = "\
[settings]
cache = false
[tasks.where]
run = [\"pwd\"]
sources = [\"data.txt\"]
[tasks.web]
cwd = \"web\"
run = [\"ls\"]
";

fn root_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::write(p.join("yatr.toml"), ROOT).unwrap();
    std::fs::write(p.join("data.txt"), "x").unwrap();
    std::fs::create_dir_all(p.join("web")).unwrap();
    std::fs::write(p.join("web/index.html"), "").unwrap();
    std::fs::create_dir_all(p.join("sub")).unwrap();
    dir
}

#[test]
fn tasks_run_in_the_config_directory_from_a_subdirectory() {
    let dir = root_project();
    let root = dir.path().canonicalize().unwrap();
    let sub = root.join("sub");

    let out = yatr(&sub, &["where"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let printed = stdout(&out);
    assert!(
        printed.lines().any(|l| Path::new(l.trim()) == root),
        "expected pwd {}: {printed}",
        root.display()
    );

    let out = yatr(&sub, &["web"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("index.html"));
}

#[test]
fn check_resolves_paths_from_a_subdirectory() {
    let dir = root_project();
    let out = yatr(&dir.path().join("sub"), &["check"]);
    assert!(out.status.success(), "stdout: {}", stdout(&out));
    assert!(!stdout(&out).contains("warning:"), "{}", stdout(&out));
}

#[test]
fn missing_task_cwd_error_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        "[tasks.web]\ncwd = \"nowhere\"\nrun = [\"ls\"]\nno_cache = true\n",
    )
    .unwrap();

    let out = yatr(dir.path(), &["web"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nowhere"), "stderr: {}", stderr(&out));
}

fn included_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::write(p.join("yatr.toml"), "include = [\"fe/yatr.toml\"]\n").unwrap();
    std::fs::create_dir_all(p.join("fe/src")).unwrap();
    std::fs::write(p.join("fe/src/app.js"), "v1").unwrap();
    std::fs::write(
        p.join("fe/yatr.toml"),
        "[tasks.fe]\nsources = [\"src/**\"]\nrun = [\"ls src\"]\n",
    )
    .unwrap();
    dir
}

#[test]
fn included_tasks_resolve_paths_against_their_own_file() {
    let dir = included_project();

    let out = yatr(dir.path(), &["fe"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("app.js"));

    let out = yatr(dir.path(), &["check"]);
    assert!(out.status.success());
    assert!(!stdout(&out).contains("warning:"), "{}", stdout(&out));
}

#[test]
fn affected_matches_included_sources_against_their_own_file() {
    let dir = included_project();
    let p = dir.path();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(p)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "tester"]);
    std::fs::write(p.join("README"), "v1").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);

    let out = yatr(p, &["affected", "HEAD", "--format", "plain"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        !stdout(&out).contains("fe"),
        "nothing changed: {}",
        stdout(&out)
    );

    std::fs::write(p.join("fe/src/app.js"), "v2").unwrap();
    for from in [p.to_path_buf(), p.join("fe")] {
        let out = yatr(&from, &["affected", "HEAD", "--format", "plain"]);
        assert!(out.status.success(), "stderr: {}", stderr(&out));
        assert!(
            stdout(&out).lines().any(|l| l == "fe"),
            "from {}: {}",
            from.display(),
            stdout(&out)
        );
    }
}

#[test]
fn relative_cache_dir_resolves_against_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(
        root.join("yatr.toml"),
        "[settings]\ncache_dir = \".cache\"\n[tasks.t]\nrun = [\"true\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();

    let out = yatr(&root.join("sub"), &["cache", "path"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(Path::new(stdout(&out).trim()), root.join(".cache"));
}
