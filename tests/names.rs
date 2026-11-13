//! Task names that collide with builtin subcommands, and the file `yatr init` writes.

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

/// A task named after a builtin cannot be reached by its bare name; `yatr check`
/// says so and names the form that does reach it.
#[test]
fn check_warns_about_tasks_shadowed_by_builtins() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        "[tasks.check]\nrun = [\"true\"]\n[tasks.help]\nrun = [\"true\"]\n[tasks.build]\nrun = [\"true\"]\n",
    )
    .unwrap();

    let out = yatr(dir.path(), &["check"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("yatr run check"), "stdout: {stdout}");
    assert!(stdout.contains("yatr run help"), "stdout: {stdout}");
    assert!(!stdout.contains("yatr run build"), "stdout: {stdout}");
}

/// `yatr init` writes a file `yatr` itself finds on a case-sensitive filesystem.
#[test]
fn init_writes_a_config_that_yatr_finds() {
    let dir = tempfile::tempdir().unwrap();

    let out = yatr(dir.path(), &["init"]);
    assert!(out.status.success());
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["yatr.toml"]);

    let out = yatr(dir.path(), &["check"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("warning:"),
        "the template should not shadow a builtin: {stdout}"
    );

    let out = yatr(dir.path(), &["init"]);
    assert!(
        !out.status.success(),
        "init must not overwrite without --force"
    );
}
