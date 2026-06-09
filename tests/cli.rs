//! End-to-end CLI tests exercising the actual `yatr` binary.

use std::io::Write;

use assert_cmd::Command;

/// `yatr schema` prints a valid draft-07 JSON Schema titled "Config".
#[test]
fn schema_prints_valid_json_schema() {
    let output = Command::cargo_bin("yatr")
        .unwrap()
        .arg("schema")
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("schema is valid JSON");
    assert_eq!(json["title"], "Config");
    assert!(json["definitions"]["TaskConfig"]["properties"]["outputs"].is_object());
}

/// `yatr run --json` emits a structured document and no human chrome.
#[test]
fn run_json_emits_structured_output() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = std::fs::File::create(dir.path().join("yatr.toml")).unwrap();
    write!(
        cfg,
        "[settings]\ncache = false\n[tasks.hello]\nrun = [\"echo hi\"]\n"
    )
    .unwrap();

    let output = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "--json", "hello"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("run --json output is valid JSON");
    assert_eq!(json["summary"]["succeeded"], 1);
    assert_eq!(json["summary"]["failed"], 0);
    assert_eq!(json["tasks"][0]["name"], "hello");
    assert_eq!(json["tasks"][0]["success"], true);
}

/// `yatr run --json --dry-run` emits the execution plan instead of running.
#[test]
fn run_json_dry_run_emits_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = std::fs::File::create(dir.path().join("yatr.toml")).unwrap();
    write!(
        cfg,
        "[tasks.a]\nrun = [\"echo a\"]\n[tasks.b]\ndepends = [\"a\"]\nrun = [\"echo b\"]\n"
    )
    .unwrap();

    let output = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "--json", "--dry-run", "b"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let order = json["plan"][0]["order"].as_array().unwrap();
    assert_eq!(order, &[serde_json::json!("a"), serde_json::json!("b")]);
}
