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

/// When one task fails, its running siblings are killed and reported cancelled.
#[cfg(unix)]
#[test]
fn failure_cancels_running_siblings() {
    let (dir, out) = run_in(
        "[tasks.slow]\nshell = true\nno_cache = true\nrun = [\"sleep 2; touch LATE\"]\n\
         [tasks.bad]\nno_cache = true\nrun = [\"false\"]\n\
         [tasks.all]\ndepends = [\"slow\", \"bad\"]\nrun = []\n",
        &["all"],
    );
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cancelled slow"), "{stdout}");
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!dir.path().join("LATE").exists(), "sibling kept running");
}

/// `--keep-going` lets independent tasks finish after a failure.
#[cfg(unix)]
#[test]
fn keep_going_finishes_independent_tasks() {
    let (dir, out) = run_in(
        "[tasks.slow]\nshell = true\nno_cache = true\nrun = [\"sleep 1; touch DONE\"]\n\
         [tasks.bad]\nno_cache = true\nrun = [\"false\"]\n\
         [tasks.after]\ndepends = [\"bad\"]\nshell = true\nno_cache = true\nrun = [\"touch AFTER\"]\n\
         [tasks.all]\ndepends = [\"slow\", \"after\"]\nrun = []\n",
        &["run", "--keep-going", "all"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(dir.path().join("DONE").exists());
    assert!(!dir.path().join("AFTER").exists());
}

/// SIGTERM to yatr reaches the task's whole process group.
#[cfg(unix)]
#[test]
fn sigterm_kills_task_processes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        "[tasks.slow]\nshell = true\nno_cache = true\nrun = [\"sleep 2; touch LATE\"]\n",
    )
    .unwrap();
    let child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yatr"))
        .current_dir(dir.path())
        .arg("slow")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(143));
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(!dir.path().join("LATE").exists(), "task outlived yatr");
}

/// `timeout` kills a task that runs too long.
#[cfg(unix)]
#[test]
fn timeout_is_enforced() {
    let start = std::time::Instant::now();
    let (_dir, out) = run_in(
        "[tasks.slow]\nno_cache = true\ntimeout = 1\nrun = [\"sleep 5\"]\n",
        &["slow"],
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(4));
    assert_eq!(out.status.code(), Some(124));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("timed out after 1s"), "{stderr}");
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("✓ cached"));
    assert_eq!(mode("out/tool"), 0o755, "a hit clobbered the exec bit");

    std::fs::remove_dir_all(p.join("out")).unwrap();
    let out = run_yatr(p, &["mk"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("✓ cached"));
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

fn yatr_project(toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        format!("[settings]\ncache_dir = \".yatr/cache\"\n{toml}"),
    )
    .unwrap();
    dir
}

fn was_cached(out: &std::process::Output) -> bool {
    String::from_utf8_lossy(&out.stdout).contains("✓ cached")
}

/// A task with no `sources` has undeclared inputs, so it is never cached.
#[test]
fn sourceless_task_is_not_cached() {
    let p = yatr_project("[tasks.gate]\nrun = [\"test -f ok\"]\n");
    std::fs::write(p.path().join("ok"), "").unwrap();
    assert!(run_yatr(p.path(), &["gate"]).status.success());
    std::fs::remove_file(p.path().join("ok")).unwrap();
    let out = run_yatr(p.path(), &["gate"]);
    assert!(
        !out.status.success(),
        "a sourceless task replayed a stale pass"
    );
    assert!(!was_cached(&out));
}

/// Two projects sharing a cache directory never serve each other's entries.
#[test]
fn identical_tasks_in_two_projects_do_not_share_local_entries() {
    let shared = tempfile::tempdir().unwrap();
    let toml = format!(
        "[settings]\ncache_dir = '{}'\n[tasks.gate]\nrun = [\"test -f ok\"]\nsources = [\"yatr.toml\"]\n",
        shared.path().display()
    );
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    std::fs::write(a.path().join("yatr.toml"), &toml).unwrap();
    std::fs::write(b.path().join("yatr.toml"), &toml).unwrap();
    std::fs::write(a.path().join("ok"), "").unwrap();

    assert!(run_yatr(a.path(), &["gate"]).status.success());
    let out = run_yatr(b.path(), &["gate"]);
    assert!(!out.status.success(), "b was served a's cached pass");
}

/// The global `[env]`, declared ambient `env_inputs` and pinned toolchains
/// are all inputs to the key, not just the task's own `env`.
#[test]
fn merged_and_declared_env_bust_the_key() {
    let echo = if cfg!(windows) {
        "echo MODE=%MODE% FOO=%FOO%"
    } else {
        "echo MODE=$MODE FOO=$FOO"
    };
    let toml = |mode: &str| {
        format!(
            "[env]\nMODE = \"{mode}\"\n[tasks.show]\nshell = true\n\
             run = [\"{echo}\"]\nsources = [\"input\"]\nenv_inputs = [\"FOO\"]\n"
        )
    };
    let p = yatr_project(&toml("one"));
    std::fs::write(p.path().join("input"), "").unwrap();
    let run = |foo: &str| {
        Command::cargo_bin("yatr")
            .unwrap()
            .current_dir(p.path())
            .env("FOO", foo)
            .arg("show")
            .output()
            .unwrap()
    };

    assert!(!was_cached(&run("1")));
    assert!(was_cached(&run("1")));
    let out = run("2");
    assert!(
        !was_cached(&out),
        "a declared env input did not bust the key"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("FOO=2"));

    std::fs::write(
        p.path().join("yatr.toml"),
        format!("[settings]\ncache_dir = \".yatr/cache\"\n{}", toml("two")),
    )
    .unwrap();
    let out = run("2");
    assert!(
        !was_cached(&out),
        "a global [env] change did not bust the key"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("MODE=two"));
}

/// A dependent re-runs when a dependency's inputs change.
#[test]
fn dependency_change_busts_dependent() {
    let p = yatr_project(
        "[tasks.gen]\nshell = true\nrun = [\"cp input.txt artifact.txt\"]\n\
         sources = [\"input.txt\"]\noutputs = [\"artifact.txt\"]\n\
         [tasks.verify]\ndepends = [\"gen\"]\nrun = [\"grep -q good artifact.txt\"]\nsources = [\"verify.sh\"]\n",
    );
    std::fs::write(p.path().join("verify.sh"), "").unwrap();
    std::fs::write(p.path().join("input.txt"), "good").unwrap();
    assert!(run_yatr(p.path(), &["verify"]).status.success());
    std::fs::write(p.path().join("input.txt"), "bad").unwrap();
    assert!(
        !run_yatr(p.path(), &["verify"]).status.success(),
        "verify replayed a result from before gen's input changed"
    );
}

/// `outputs = ["out/**"]` captures the tree, and is restored on a hit.
#[test]
fn double_star_output_is_captured_and_restored() {
    let gen = if cfg!(windows) {
        r"mkdir out\sub && echo hi> out\a.txt && echo hi> out\sub\b.txt"
    } else {
        "mkdir -p out/sub && echo hi > out/a.txt && echo hi > out/sub/b.txt"
    };
    let p = yatr_project(&format!(
        "[tasks.gen]\nshell = true\nrun = ['{gen}']\n\
         sources = [\"input\"]\noutputs = [\"out/**\"]\n"
    ));
    std::fs::write(p.path().join("input"), "").unwrap();
    assert!(run_yatr(p.path(), &["gen"]).status.success());
    std::fs::remove_dir_all(p.path().join("out")).unwrap();
    let out = run_yatr(p.path(), &["gen"]);
    assert!(was_cached(&out));
    assert!(
        p.path().join("out/a.txt").exists() && p.path().join("out/sub/b.txt").exists(),
        "a hit on `out/**` did not restore the tree"
    );
}

/// `*` in `sources` stops at `/`, as it does in `outputs`, and `./` is ignored.
#[test]
fn source_globs_share_the_output_dialect() {
    let p = yatr_project("[tasks.t]\nrun = [\"true\"]\nsources = [\"*.txt\", \"./data.bin\"]\n");
    std::fs::write(p.path().join("a.txt"), "").unwrap();
    std::fs::write(p.path().join("data.bin"), "v1").unwrap();
    assert!(!was_cached(&run_yatr(p.path(), &["t"])));

    std::fs::create_dir(p.path().join("sub")).unwrap();
    std::fs::write(p.path().join("sub/deep.txt"), "").unwrap();
    assert!(
        was_cached(&run_yatr(p.path(), &["t"])),
        "`*.txt` matched a file in a subdirectory"
    );

    std::fs::write(p.path().join("data.bin"), "v2").unwrap();
    assert!(
        !was_cached(&run_yatr(p.path(), &["t"])),
        "a `./` source was not hashed"
    );
}

/// A source named through a gitignored path is generated input, and is hashed.
#[test]
fn gitignored_source_named_explicitly_is_hashed() {
    let p = yatr_project(
        "[tasks.pack]\nrun = [\"true\"]\nsources = [\"gen/schema.json\", \"dist/**\"]\n",
    );
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(p.path())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    std::fs::write(p.path().join(".gitignore"), "gen/\ndist/\n").unwrap();
    std::fs::create_dir_all(p.path().join("gen")).unwrap();
    std::fs::create_dir_all(p.path().join("dist/js")).unwrap();
    std::fs::write(p.path().join("gen/schema.json"), "v1").unwrap();
    std::fs::write(p.path().join("dist/js/app.js"), "v1").unwrap();

    let check = run_yatr(p.path(), &["check"]);
    assert!(
        !String::from_utf8_lossy(&check.stdout).contains("no files")
            && !String::from_utf8_lossy(&check.stderr).contains("no files"),
        "check reported an existing ignored source as missing"
    );

    assert!(!was_cached(&run_yatr(p.path(), &["pack"])));
    assert!(was_cached(&run_yatr(p.path(), &["pack"])));
    std::fs::write(p.path().join("gen/schema.json"), "v2").unwrap();
    assert!(!was_cached(&run_yatr(p.path(), &["pack"])));
    assert!(was_cached(&run_yatr(p.path(), &["pack"])));
    std::fs::write(p.path().join("dist/js/app.js"), "v2").unwrap();
    assert!(!was_cached(&run_yatr(p.path(), &["pack"])));
}

/// Outside a git checkout `.gitignore` still keeps ignored files out of a key.
#[test]
fn gitignore_is_honoured_outside_git() {
    let p = yatr_project("[tasks.t]\nrun = [\"true\"]\nsources = [\"**/*.rs\"]\n");
    std::fs::write(p.path().join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir(p.path().join("target")).unwrap();
    std::fs::write(p.path().join("lib.rs"), "").unwrap();
    std::fs::write(p.path().join("target/gen.rs"), "v1").unwrap();
    assert!(!was_cached(&run_yatr(p.path(), &["t"])));
    std::fs::write(p.path().join("target/gen.rs"), "v2").unwrap();
    assert!(
        was_cached(&run_yatr(p.path(), &["t"])),
        "an ignored file busted the key"
    );
}
