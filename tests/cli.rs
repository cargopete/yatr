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

/// Write `toml` as the yatr.toml of a fresh temp dir and run `yatr` there.
fn run_in(toml: &str, args: &[&str]) -> (tempfile::TempDir, std::process::Output) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("yatr.toml"), toml).unwrap();
    let out = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .args(args)
        .output()
        .unwrap();
    (dir, out)
}

/// A failing task shows what it printed and reports its real exit code.
#[cfg(unix)]
#[test]
fn failing_task_shows_output_and_real_exit_code() {
    let (_dir, out) = run_in(
        "[tasks.boom]\nshell = true\nno_cache = true\n\
         run = [\"echo 'IMPORTANT DIAGNOSTIC LINE'; echo 'to stderr' >&2; exit 3\"]\n",
        &["boom"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}{stderr}");
    assert_eq!(out.status.code(), Some(3), "{all}");
    assert!(all.contains("IMPORTANT DIAGNOSTIC LINE"), "{all}");
    assert!(all.contains("to stderr"), "{all}");
    assert!(
        stderr.contains("Task 'boom' failed with exit code 3"),
        "{stderr}"
    );
    assert!(!all.contains("exit code 1"), "{all}");
}

/// `--json` still emits its document when a task fails, with the output and code.
#[cfg(unix)]
#[test]
fn failing_task_json_carries_output_and_code() {
    let (_dir, out) = run_in(
        "[tasks.boom]\nshell = true\nno_cache = true\nrun = [\"echo diag; exit 7\"]\n",
        &["run", "--json", "boom"],
    );
    assert_eq!(out.status.code(), Some(7));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["tasks"][0]["success"], false);
    assert_eq!(json["tasks"][0]["exit_code"], 7);
    assert!(json["tasks"][0]["output"]
        .as_str()
        .unwrap()
        .contains("diag"));
}

/// Output is streamed while the task runs, not held back until it exits.
#[cfg(unix)]
#[test]
fn output_streams_while_task_runs() {
    use std::io::BufRead;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        "[tasks.slow]\nshell = true\nno_cache = true\nrun = [\"echo first; sleep 3; echo second\"]\n",
    )
    .unwrap();
    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yatr"))
        .current_dir(dir.path())
        .arg("slow")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let first = stdout
        .lines()
        .map_while(std::result::Result::ok)
        .find(|l| l.contains("first"));
    let seen_after = start.elapsed();
    child.wait().unwrap();
    assert!(first.is_some());
    assert!(
        seen_after < std::time::Duration::from_secs(2),
        "first line arrived only after {seen_after:?}"
    );
}

/// stderr is shown for a successful task too.
#[cfg(unix)]
#[test]
fn successful_task_shows_stderr() {
    let (_dir, out) = run_in(
        "[tasks.warn]\nshell = true\nno_cache = true\nrun = [\"echo out; echo 'warning: err' >&2\"]\n",
        &["warn"],
    );
    assert!(out.status.success());
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("out") && all.contains("warning: err"), "{all}");
}

/// Leading whitespace survives, both live and when replayed from the cache.
#[cfg(unix)]
#[test]
fn output_keeps_leading_whitespace_live_and_cached() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("in.txt"), "x").unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        "[tasks.pad]\nshell = true\nsources = [\"in.txt\"]\n\
         run = [\"printf '   right\\\\n   aligned\\\\n'\"]\n",
    )
    .unwrap();
    for run in ["live", "cached"] {
        let out = Command::cargo_bin("yatr")
            .unwrap()
            .current_dir(dir.path())
            .arg("pad")
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        let tail = |word: &str| {
            stdout
                .lines()
                .find(|l| l.ends_with(word))
                .map(|l| l[..l.len() - word.len()].to_string())
        };
        let (right, aligned) = (tail("right"), tail("aligned"));
        assert!(right.is_some(), "{run}: {stdout}");
        assert_eq!(right, aligned, "{run}: {stdout}");
        assert!(right.unwrap().ends_with("   "), "{run}: {stdout}");
    }
    let cached = Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir.path())
        .arg("pad")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&cached.stdout).contains("cached"));
}

/// A reader that closes stdout early does not make yatr panic.
#[cfg(unix)]
#[test]
fn closed_stdout_is_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        "[tasks.pad]\nshell = true\nno_cache = true\nrun = [\"sleep 0.5; seq 1 2000\"]\n",
    )
    .unwrap();
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yatr"))
        .current_dir(dir.path())
        .arg("pad")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(0), "{stderr}");
}
