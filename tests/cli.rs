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

/// `yatr run --profile` writes a valid Chrome trace with one event per task.
#[test]
fn run_profile_writes_chrome_trace() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = std::fs::File::create(dir.path().join("yatr.toml")).unwrap();
    write!(
        cfg,
        "[settings]\ncache = false\n[tasks.a]\nrun = [\"echo a\"]\n[tasks.b]\ndepends = [\"a\"]\nrun = [\"echo b\"]\n"
    )
    .unwrap();

    let trace = dir.path().join("trace.json");
    let output = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "--profile"])
        .arg(&trace)
        .arg("b")
        .output()
        .unwrap();
    assert!(output.status.success());

    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&trace).unwrap()).expect("trace is valid JSON");
    let events = json["traceEvents"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    // Chrome trace events carry a name, a duration, and the "X" (complete) phase.
    assert!(events
        .iter()
        .all(|e| e["ph"] == "X" && e["dur"].is_number()));
    let names: Vec<&str> = events.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"a") && names.contains(&"b"));
}

/// `yatr affected <ref>` lists only tasks whose sources changed since the ref.
#[test]
fn affected_lists_tasks_touched_by_changes() {
    let dir = tempfile::tempdir().unwrap();
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

    std::fs::create_dir_all(p.join("web")).unwrap();
    std::fs::create_dir_all(p.join("api")).unwrap();
    std::fs::write(p.join("web/app.js"), "v1").unwrap();
    std::fs::write(p.join("api/main.rs"), "v1").unwrap();
    std::fs::write(
        p.join("yatr.toml"),
        "[tasks.frontend]\nsources=[\"web/**\"]\nrun=[\"echo fe\"]\n\
         [tasks.backend]\nsources=[\"api/**\"]\nrun=[\"echo be\"]\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);

    // Change only a frontend source.
    std::fs::write(p.join("web/app.js"), "v2").unwrap();

    let out = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(p)
        .args(["affected", "HEAD", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let aff: Vec<&str> = json["affected"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        aff.contains(&"frontend"),
        "frontend should be affected: {aff:?}"
    );
    assert!(
        !aff.contains(&"backend"),
        "backend should not be affected: {aff:?}"
    );
}

/// A task backed by a WASM plugin runs and its emitted output is captured.
#[test]
fn run_executes_wasm_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let wasm = wat::parse_str(
        r#"(module
            (import "yatr" "emit" (func $emit (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "from plugin")
            (func (export "run") (result i32)
                (call $emit (i32.const 0) (i32.const 11))
                (i32.const 0)))"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("p.wasm"), wasm).unwrap();

    let mut cfg = std::fs::File::create(dir.path().join("yatr.toml")).unwrap();
    write!(
        cfg,
        "[settings]\ncache = false\n[tasks.gen]\nwasm = \"p.wasm\"\n"
    )
    .unwrap();

    let out = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "gen"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("from plugin"), "stdout: {stdout}");
}

/// A WASM plugin receives the task's env as input and can read it back.
#[test]
fn wasm_plugin_reads_task_env() {
    let dir = tempfile::tempdir().unwrap();
    // Reads the input JSON into memory and emits it back verbatim.
    let wasm = wat::parse_str(
        r#"(module
            (import "yatr" "emit" (func $emit (param i32 i32)))
            (import "yatr" "input_len" (func $input_len (result i32)))
            (import "yatr" "input_read" (func $input_read (param i32) (result i32)))
            (memory (export "memory") 1)
            (func (export "run") (result i32)
                (local $len i32)
                (local.set $len (call $input_len))
                (drop (call $input_read (i32.const 256)))
                (call $emit (i32.const 256) (local.get $len))
                (i32.const 0)))"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("p.wasm"), wasm).unwrap();

    let mut cfg = std::fs::File::create(dir.path().join("yatr.toml")).unwrap();
    write!(
        cfg,
        "[settings]\ncache = false\n[tasks.echo-env]\nwasm = \"p.wasm\"\nenv = {{ GREETING = \"bonjour\" }}\n"
    )
    .unwrap();

    let out = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .args(["run", "echo-env"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("GREETING") && stdout.contains("bonjour"),
        "plugin should see task env: {stdout}"
    );
}

/// `yatr check` errors on a missing referenced file and warns on config smells.
#[test]
fn check_validates_files_and_warns() {
    // A wasm task pointing at a non-existent plugin → check fails.
    let bad = tempfile::tempdir().unwrap();
    std::fs::write(
        bad.path().join("yatr.toml"),
        "[tasks.gen]\nwasm = \"nope.wasm\"\n",
    )
    .unwrap();
    let out = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(bad.path())
        .arg("check")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "check should fail on a missing plugin"
    );

    // A valid-but-smelly config → succeeds, with a warning.
    let ok = tempfile::tempdir().unwrap();
    std::fs::write(
        ok.path().join("yatr.toml"),
        "[tasks.build]\nrun = [\"echo hi\"]\noutputs = [\"dist\"]\nno_cache = true\n",
    )
    .unwrap();
    let out = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(ok.path())
        .arg("check")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("warning:"), "expected a warning: {stdout}");
}

fn run_yatr(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

/// A cache hit must restore outputs with their mode and symlinks intact, and
/// must not strip the executable bit from an output that is already present.
#[cfg(unix)]
#[test]
fn cache_hit_restores_exec_bit_and_symlinks() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::write(
        p.join("yatr.toml"),
        "[settings]\ncache_dir = \".yatr/cache\"\n\
         [tasks.mk]\nshell = true\n\
         run = [\"mkdir -p out && printf '#!/bin/sh\\\\necho hi\\\\n' > out/tool && chmod +x out/tool && ln -sf tool out/link\"]\n\
         sources = [\"yatr.toml\"]\noutputs = [\"out\"]\n\
         [tasks.mkglob]\nshell = true\n\
         run = [\"mkdir -p g && printf x > g/f && ln -sf f g/link\"]\n\
         sources = [\"yatr.toml\"]\noutputs = [\"g/*\"]\n",
    )
    .unwrap();
    let mode = |rel: &str| std::fs::metadata(p.join(rel)).unwrap().permissions().mode() & 0o777;

    assert!(run_yatr(p, &["mk"]).status.success());
    // Second run is a hit over the existing outputs.
    let out = run_yatr(p, &["mk"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("cached"));
    assert_eq!(mode("out/tool"), 0o755, "a hit clobbered the exec bit");

    std::fs::remove_dir_all(p.join("out")).unwrap();
    let out = run_yatr(p, &["mk"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("cached"));
    assert_eq!(mode("out/tool"), 0o755, "restored without its exec bit");
    let link = p.join("out/link");
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        std::path::Path::new("tool")
    );

    assert!(run_yatr(p, &["mkglob"]).status.success());
    std::fs::remove_dir_all(p.join("g")).unwrap();
    assert!(run_yatr(p, &["mkglob"]).status.success());
    let link = p.join("g/link");
    assert!(
        link.symlink_metadata().unwrap().file_type().is_symlink(),
        "a globbed symlink came back as a regular file"
    );
}
