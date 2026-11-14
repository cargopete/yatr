//! The surfaces a coding agent uses: `--agent`, `--ndjson`, `--tail`, JSON
//! errors, `explain`, `agents` and the MCP server.

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use assert_cmd::Command;
use serde_json::Value;

fn project(toml: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("yatr.toml"),
        format!("[settings]\ncache_dir = \".yatr/cache\"\n{toml}"),
    )
    .unwrap();
    dir
}

fn yatr(dir: &Path, args: &[&str]) -> Output {
    Command::cargo_bin("yatr")
        .unwrap()
        .current_dir(dir)
        .env_remove("YATR_AGENT")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout is one JSON document")
}

const NOISY_FAILURE: &str = r#"
[tasks.quiet]
run = ["echo fine"]

[tasks.noisy]
depends = ["quiet"]
run = ["sh -c 'for i in 1 2 3 4 5 6; do echo line $i; done; exit 3'"]
"#;

/// A failure is one line, the tail of its output and where the rest is; a
/// pass is one line and nothing else.
#[cfg(unix)]
#[test]
fn agent_report_bounds_a_failure_and_logs_the_rest() {
    let p = project(NOISY_FAILURE);
    let out = yatr(p.path(), &["run", "--agent", "--tail", "2", "noisy"]);
    assert_eq!(out.status.code(), Some(3));

    let text = stdout(&out);
    assert!(text.contains("ok   quiet"), "{text}");
    assert!(!text.contains("fine"), "a passing task's output is noise");
    assert!(text.contains("FAIL noisy"), "{text}");
    assert!(text.contains("exit 3 from `sh -c"), "{text}");
    assert!(text.contains("last 2 of 6 lines, full log: .yatr/logs/noisy.log"));
    assert!(text.contains("line 5\nline 6\n") && !text.contains("line 4"));
    assert!(text.contains("FAILED: 1 failed, 1 ok"), "{text}");
    assert!(text.contains("rerun: yatr run --agent noisy"), "{text}");
    assert!(
        out.stderr.is_empty(),
        "the report already names the failure"
    );

    let log = std::fs::read_to_string(p.path().join(".yatr/logs/noisy.log")).unwrap();
    assert_eq!(log.lines().count(), 6);
    assert_eq!(
        std::fs::read_to_string(p.path().join(".yatr/.gitignore")).unwrap(),
        "*\n"
    );
}

/// `YATR_AGENT=1` asks for the agent report without the flag, and a JSON
/// flag given by name still wins.
#[test]
fn agent_env_var_selects_the_agent_report() {
    let p = project("[tasks.hello]\nrun = [\"echo hi\"]\n");
    let run = |args: &[&str]| {
        Command::cargo_bin("yatr")
            .unwrap()
            .current_dir(p.path())
            .env("YATR_AGENT", "1")
            .args(args)
            .output()
            .unwrap()
    };
    let text = stdout(&run(&["hello"]));
    assert!(text.starts_with("ok   hello"), "{text}");
    assert!(text.contains("OK: 1 ok"), "{text}");

    assert_eq!(json(&run(&["run", "--json", "hello"]))["ok"], true);
}

/// A failure that is allowed does not fail the run or count as one.
#[cfg(unix)]
#[test]
fn agent_report_marks_an_allowed_failure() {
    let p = project(
        "[tasks.flaky]\nrun = [\"sh -c 'exit 2'\"]\nallow_failure = true\n\
         [tasks.all]\ndepends = [\"flaky\"]\n",
    );
    let out = yatr(p.path(), &["run", "--agent", "all"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("warn flaky"), "{text}");
    assert!(text.contains("OK: 0 ok, 1 allowed to fail"), "{text}");
}

/// An error before anything runs is still JSON on stdout for a JSON reader,
/// and carries the near misses.
#[test]
fn json_error_names_the_near_miss() {
    let p = project("[tasks.build]\nrun = [\"echo hi\"]\n");
    for args in [
        &["run", "--json", "biuld"][..],
        &["explain", "--format", "json", "biuld"],
    ] {
        let out = yatr(p.path(), args);
        assert_eq!(out.status.code(), Some(1));
        let error = &json(&out)["error"];
        assert_eq!(error["code"], "yatr::task::not_found");
        assert_eq!(error["suggestions"][0], "build");
        assert_eq!(error["available"][0], "build");
    }

    let empty = tempfile::tempdir().unwrap();
    let out = yatr(empty.path(), &["list", "--format", "json"]);
    assert_eq!(json(&out)["error"]["code"], "yatr::config::not_found");
}

/// `--tail` shortens `output` and says so; a failed run's document is not
/// followed by a second one for the error.
#[cfg(unix)]
#[test]
fn json_tail_shortens_output_and_keeps_one_document() {
    let p = project(NOISY_FAILURE);
    let out = yatr(p.path(), &["run", "--json", "--tail", "2", "noisy"]);
    assert_eq!(out.status.code(), Some(3));

    let doc = json(&out);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["error"]["code"], "yatr::exec::failed");
    let noisy = &doc["tasks"][1];
    assert_eq!(noisy["output"], "line 5\nline 6");
    assert_eq!(noisy["output_lines"], 6);
    assert_eq!(noisy["output_truncated"], true);
    assert_eq!(noisy["log"], ".yatr/logs/noisy.log");
    assert_eq!(noisy["cache"], "uncacheable");
    assert!(noisy["command"].as_str().unwrap().starts_with("sh -c"));

    // Without `--tail` the output is whole and no log is written.
    let p = project(NOISY_FAILURE);
    let doc = json(&yatr(p.path(), &["run", "--json", "noisy"]));
    assert_eq!(
        doc["tasks"][1]["output"].as_str().unwrap().lines().count(),
        6
    );
    assert!(doc["tasks"][1].get("log").is_none());
    assert!(!p.path().join(".yatr/logs").exists());
}

/// `--ndjson` is one event per line, from `run_start` to `run_finish`.
#[test]
fn ndjson_streams_the_run_as_events() {
    let p = project(
        "[tasks.a]\nrun = [\"echo one\"]\n[tasks.b]\ndepends = [\"a\"]\nrun = [\"echo two\"]\n",
    );
    let out = yatr(p.path(), &["run", "--ndjson", "-v", "b"]);
    assert!(out.status.success());

    let events: Vec<Value> = stdout(&out)
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "run_start",
            "task_start",
            "output",
            "task_finish",
            "task_start",
            "output",
            "task_finish",
            "run_finish"
        ]
    );
    assert_eq!(events[0]["tasks"], serde_json::json!(["a", "b"]));
    assert_eq!(events[2]["line"], "one");
    assert_eq!(events[3]["name"], "a");
    assert_eq!(events[7]["ok"], true);

    let out = yatr(p.path(), &["run", "--ndjson", "nope"]);
    let event: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(event["event"], "error");
}

/// `explain` reaches the verdict a run would, and names what changed.
#[test]
fn explain_names_the_inputs_that_changed() {
    let p = project(
        "[tasks.gen]\nrun = [\"echo gen\"]\nsources = [\"in/**\"]\n\
         [tasks.build]\ndepends = [\"gen\"]\nrun = [\"echo build\"]\nsources = [\"src/**\"]\n\
         [tasks.fmt]\nrun = [\"echo fmt\"]\nno_cache = true\n",
    );
    for (path, text) in [("in/a.txt", "a"), ("src/main.rs", "fn main() {}")] {
        let path = p.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let explain = |task: &str| json(&yatr(p.path(), &["explain", "--format", "json", task]));

    let before = explain("build");
    assert_eq!(before["tasks"][0]["name"], "build");
    assert_eq!(before["tasks"][0]["decision"], "runs");
    assert!(before["tasks"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("no record"));

    assert!(yatr(p.path(), &["build"]).status.success());
    let hit = explain("build");
    assert_eq!(hit["tasks"][0]["decision"], "cached");
    assert_eq!(hit["tasks"][1]["decision"], "cached");
    assert!(hit["tasks"][0]["saved_ms"].is_u64());

    // A change to the dependency's source reaches the dependent through it.
    std::fs::write(p.path().join("in/a.txt"), "changed").unwrap();
    std::fs::write(p.path().join("src/new.rs"), "").unwrap();
    let miss = explain("build");
    let build = &miss["tasks"][0];
    assert_eq!(build["decision"], "runs");
    assert_eq!(build["changes"][0]["input"], "sources");
    assert_eq!(build["changes"][0]["items"][0], "added src/new.rs");
    assert_eq!(build["changes"][1]["input"], "dependencies");
    assert_eq!(build["changes"][1]["items"][0], "changed gen");
    assert_eq!(
        miss["tasks"][1]["changes"][0]["items"][0],
        "modified in/a.txt"
    );

    // The verdict is the one the run reaches.
    let run = json(&yatr(p.path(), &["run", "--json", "build"]));
    assert_eq!(run["tasks"][0]["cache"], "miss");
    assert_eq!(run["tasks"][1]["cache"], "miss");
    assert_eq!(explain("build")["tasks"][0]["decision"], "cached");

    assert_eq!(explain("fmt")["tasks"][0]["decision"], "always_runs");
    let text = stdout(&yatr(p.path(), &["explain", "fmt"]));
    assert_eq!(text, "fmt: always runs - `no_cache = true`\n");
}

const AGENTS_PROJECT: &str = r#"
[tasks.test]
desc = "Run the tests | all of them"
run = ["echo test"]
sources = ["src/**"]

[tasks.dev]
desc = "Dev server"
run = ["echo serve"]
foreground = true

[tasks.ci]
desc = "The gate"
depends = ["test"]
"#;

/// `agents --write` keeps what surrounds its section, `--check` notices drift.
#[test]
fn agents_section_is_written_in_place_and_checked() {
    let p = project(AGENTS_PROJECT);
    let agents_md = p.path().join("AGENTS.md");

    let section = stdout(&yatr(p.path(), &["agents"]));
    assert!(section.starts_with("<!-- yatr:begin -->\n"));
    assert!(section.ends_with("<!-- yatr:end -->\n"));
    assert!(section.contains("run `yatr run --agent ci`"));
    assert!(
        section.contains("| `test` | Run the tests \\| all of them |  | cached on its inputs |")
    );
    assert!(section.contains("| `ci` | The gate | `test` | runs its dependencies only |"));
    assert!(section.contains("| `dev` | Dev server |  | long-running"));

    assert!(!yatr(p.path(), &["agents", "--check"]).status.success());

    std::fs::write(&agents_md, "# Notes\n\nMine.\n").unwrap();
    assert!(yatr(p.path(), &["agents", "--write"]).status.success());
    assert_eq!(
        std::fs::read_to_string(&agents_md).unwrap(),
        format!("# Notes\n\nMine.\n\n{section}")
    );
    assert!(yatr(p.path(), &["agents", "--check"]).status.success());

    // A new task makes the section stale until it is written again.
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(p.path().join("yatr.toml"))
        .unwrap();
    writeln!(config, "[tasks.lint]\nrun = [\"echo lint\"]").unwrap();
    let stale = yatr(p.path(), &["agents", "--check"]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("AGENTS.md is out of date"));

    assert!(yatr(p.path(), &["agents", "--write"]).status.success());
    let written = std::fs::read_to_string(&agents_md).unwrap();
    assert!(written.starts_with("# Notes\n\nMine.\n\n<!-- yatr:begin -->"));
    assert!(written.contains("| `lint` |"));
    assert_eq!(written.matches("yatr:begin").count(), 1);

    // Kept in another file, the section says so, and is checked against that.
    assert!(yatr(p.path(), &["agents", "--write", "CLAUDE.md"])
        .status
        .success());
    let claude_md = std::fs::read_to_string(p.path().join("CLAUDE.md")).unwrap();
    assert!(claude_md.contains("`yatr agents --write CLAUDE.md`"));
    assert!(yatr(p.path(), &["agents", "--check", "CLAUDE.md"])
        .status
        .success());
}

/// `list --format json` says enough to choose a task without reading the config.
#[test]
fn list_json_describes_each_task() {
    let p = project(AGENTS_PROJECT);
    let tasks = json(&yatr(p.path(), &["list", "--format", "json"]));
    assert_eq!(tasks["test"]["kind"], "commands");
    assert_eq!(tasks["test"]["cached"], true);
    assert_eq!(tasks["test"]["run"][0], "echo test");
    assert_eq!(tasks["test"]["sources"][0], "src/**");
    assert_eq!(tasks["ci"]["kind"], "aggregate");
    assert_eq!(tasks["ci"]["depends"][0], "test");
    assert_eq!(tasks["dev"]["foreground"], true);
    assert_eq!(tasks["dev"]["cached"], false);
}

/// A task named after a subcommand added since 1.0 is still what the bare
/// form runs, and `check` does not claim otherwise.
#[test]
fn a_task_keeps_its_name_from_a_newer_builtin() {
    let p = project("[tasks.explain]\nrun = [\"echo mine\"]\nno_cache = true\n");
    for args in [
        &["explain"][..],
        &["-q", "explain"],
        &["--tail", "3", "explain"],
    ] {
        let out = yatr(p.path(), args);
        assert!(out.status.success(), "{args:?}");
        assert_eq!(stdout(&out), "mine\n", "{args:?}");
    }
    let check = stdout(&yatr(p.path(), &["check"]));
    assert!(!check.contains("builtin"), "{check}");

    // Without such a task the builtin is what runs, options ahead of it or not.
    let p = project("[tasks.build]\nrun = [\"echo hi\"]\nno_cache = true\n");
    for args in [
        &["explain", "build"][..],
        &["--no-color", "explain", "build"],
    ] {
        let out = yatr(p.path(), args);
        assert!(stdout(&out).starts_with("build: always runs"), "{args:?}");
    }
}

/// An option ahead of a subcommand does not turn the subcommand into a task
/// name, unless there is a task of that name, which is what ran before.
#[test]
fn options_may_come_before_a_subcommand() {
    let p = project("[tasks.build]\nrun = [\"echo hi\"]\nno_cache = true\n");
    let out = yatr(p.path(), &["-c", "yatr.toml", "list", "--format", "plain"]);
    assert_eq!(stdout(&out), "build\n");
    let out = yatr(p.path(), &["-q", "--parallel", "2", "run", "build"]);
    assert_eq!(stdout(&out), "hi\n");

    let sub = p.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let out = yatr(&sub, &["--cwd", "..", "list", "--format", "plain"]);
    assert_eq!(stdout(&out), "build\n");

    let p = project("[tasks.list]\nrun = [\"echo mine\"]\nno_cache = true\n");
    assert_eq!(stdout(&yatr(p.path(), &["-q", "list"])), "mine\n");
    assert!(stdout(&yatr(p.path(), &["list"])).contains("Available tasks"));
}

/// Send `requests` to `yatr mcp` and return its responses keyed by id. The
/// server stops what is running when stdin closes, so it stays open until
/// every request has its answer.
fn mcp(dir: &Path, requests: &[Value]) -> std::collections::HashMap<u64, Value> {
    use std::io::BufRead;

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("yatr"))
        .current_dir(dir)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    let expected = requests.iter().filter(|r| r.get("id").is_some()).count();
    let mut responses = std::collections::HashMap::new();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    while responses.len() < expected {
        let line = lines.next().expect("a response per request").unwrap();
        let message: Value = serde_json::from_str(&line).expect("each line is one message");
        responses.insert(message["id"].as_u64().unwrap(), message);
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
    responses
}

fn request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    request(
        id,
        "tools/call",
        serde_json::json!({ "name": name, "arguments": arguments }),
    )
}

/// The server answers the handshake, lists its tools and runs tasks,
/// returning a failure's output and dropping a pass's.
#[cfg(unix)]
#[test]
fn mcp_server_lists_and_runs_tasks() {
    let p = project(NOISY_FAILURE);
    let responses = mcp(
        p.path(),
        &[
            request(
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" },
                }),
            ),
            serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            request(2, "tools/list", serde_json::json!({})),
            tool_call(3, "list", serde_json::json!({})),
            tool_call(
                4,
                "run",
                serde_json::json!({ "tasks": ["noisy"], "tail": 2 }),
            ),
            tool_call(5, "run", serde_json::json!({ "tasks": ["--force"] })),
            tool_call(6, "explain", serde_json::json!({ "task": "quiet" })),
            request(7, "resources/list", serde_json::json!({})),
        ],
    );

    let init = &responses[&1]["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "yatr");
    assert!(init["capabilities"]["tools"].is_object());

    let names: Vec<&str> = responses[&2]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["list", "run", "explain", "affected", "check"]);

    let list = &responses[&3]["result"];
    assert_eq!(list["isError"], false);
    assert_eq!(list["structuredContent"]["noisy"]["depends"][0], "quiet");

    let run = &responses[&4]["result"];
    assert_eq!(run["isError"], true);
    let tasks = &run["structuredContent"]["tasks"];
    assert!(tasks[0].get("output").is_none(), "a pass carries no output");
    assert_eq!(tasks[1]["output"], "line 5\nline 6");
    assert_eq!(tasks[1]["exit_code"], 3);
    let text: Value = serde_json::from_str(run["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&text, &run["structuredContent"]);

    // A task name cannot smuggle an option in.
    let smuggled = &responses[&5]["result"];
    assert_eq!(smuggled["isError"], true);
    assert_eq!(
        smuggled["structuredContent"]["error"]["code"],
        "yatr::task::not_found"
    );

    assert_eq!(
        responses[&6]["result"]["structuredContent"]["tasks"][0]["decision"],
        "always_runs"
    );
    assert_eq!(responses[&7]["error"]["code"], -32601);
}

/// A foreground task would hold the call open for ever, so it is refused.
#[test]
fn mcp_server_refuses_a_foreground_task() {
    let p = project(AGENTS_PROJECT);
    let responses = mcp(
        p.path(),
        &[tool_call(1, "run", serde_json::json!({ "tasks": ["dev"] }))],
    );
    let result = &responses[&1]["result"];
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("foreground task"));
}
