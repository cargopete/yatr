//! `yatr mcp`: a Model Context Protocol server over stdio.
//!
//! Each tool runs `yatr` itself as a child process, so every call reads the
//! config afresh and nothing a task prints can reach the protocol stream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::Result;
use crate::graph::TaskGraph;
use crate::report::AGENT_TAIL;

/// Protocol revisions this server speaks; the last is offered to a client
/// that asks for one it does not know.
const PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-11-25", "2025-06-18"];

const INSTRUCTIONS: &str = "This project's build, test and lint commands are yatr tasks. \
    Call `list` to see them, and `run` to execute them: results are cached on their inputs, \
    so running a task whose inputs have not changed costs nothing. `explain` says why a task \
    would or would not run.";

type Out = Arc<Mutex<tokio::io::Stdout>>;
type Running = Arc<std::sync::Mutex<HashMap<String, CancellationToken>>>;

fn tools() -> Value {
    let read_only = json!({ "readOnlyHint": true });
    json!([
        {
            "name": "list",
            "title": "List tasks",
            "description": "List the project's tasks: what each does, what it depends on, \
                the commands it runs, and whether its result is cached.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
        {
            "name": "run",
            "title": "Run tasks",
            "description": "Run tasks and their dependencies. Returns each task's result; a \
                failed task carries the failing command, its exit code and the tail of its \
                output, and `log` names the file holding all of it. A task whose inputs are \
                unchanged is restored from cache rather than run.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Task names, as `list` gives them.",
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Run even where a cached result exists.",
                    },
                    "keep_going": {
                        "type": "boolean",
                        "description": "After a failure, still run the tasks that do not depend on it.",
                    },
                    "affected": {
                        "type": "string",
                        "description": "A git ref: run only those of the tasks that changes since it can affect.",
                    },
                    "tail": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Lines of a failed task's output to return (default 40).",
                    },
                },
                "required": ["tasks"],
            },
        },
        {
            "name": "explain",
            "title": "Explain a cache decision",
            "description": "Say whether a task would run or be restored from cache, and which \
                of its inputs changed since it last ran. Runs nothing.",
            "inputSchema": {
                "type": "object",
                "properties": { "task": { "type": "string" } },
                "required": ["task"],
            },
            "annotations": read_only,
        },
        {
            "name": "affected",
            "title": "List affected tasks",
            "description": "List the tasks that changes since a git ref can affect.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "git_ref": {
                        "type": "string",
                        "description": "For example `origin/main` or `HEAD~1`.",
                    },
                },
                "required": ["git_ref"],
            },
            "annotations": read_only,
        },
        {
            "name": "check",
            "title": "Validate the config",
            "description": "Validate yatr.toml and report its errors and warnings. Call it after editing the file.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": read_only,
        },
    ])
}

async fn send(out: &Out, message: &Value) {
    let mut line = serde_json::to_vec(message).unwrap_or_default();
    line.push(b'\n');
    let mut out = out.lock().await;
    let _ = out.write_all(&line).await;
    let _ = out.flush().await;
}

fn response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: &Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_result(text: &str, structured: Option<Value>, is_error: bool) -> Value {
    let mut result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    });
    if let Some(structured) = structured.filter(Value::is_object) {
        result["structuredContent"] = structured;
    }
    result
}

fn tool_error(message: impl AsRef<str>) -> Value {
    tool_result(message.as_ref(), None, true)
}

/// A string argument that cannot be mistaken for an option.
fn string_arg<'a>(args: &'a Value, name: &str) -> std::result::Result<&'a str, Value> {
    match args.get(name).and_then(Value::as_str) {
        Some(value) if !value.is_empty() && !value.starts_with('-') => Ok(value),
        Some(value) => Err(tool_error(format!("`{name}` is not valid: {value:?}"))),
        None => Err(tool_error(format!(
            "`{name}` is required and must be a string"
        ))),
    }
}

/// The `yatr` arguments for a tool call, or the error to return in their place.
fn arguments(
    name: &str,
    args: &Value,
    config: Option<&Path>,
) -> std::result::Result<Vec<String>, Value> {
    let flag = |name: &str| args.get(name).and_then(Value::as_bool).unwrap_or(false);
    let own = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

    match name {
        "list" => Ok(own(&["list", "--format", "json"])),
        "check" => Ok(own(&["check"])),
        "explain" => {
            let task = string_arg(args, "task")?;
            Ok(own(&["explain", "--format", "json", "--", task]))
        }
        "affected" => {
            let git_ref = string_arg(args, "git_ref")?;
            Ok(own(&["affected", "--format", "json", "--", git_ref]))
        }
        "run" => {
            let tasks: Vec<&str> = args
                .get("tasks")
                .and_then(Value::as_array)
                .map(|tasks| tasks.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if tasks.is_empty() || tasks.iter().any(|t| t.is_empty()) {
                return Err(tool_error(
                    "`tasks` must be a non-empty array of task names",
                ));
            }
            if let Some(task) = tasks.iter().find(|t| t.starts_with('-')) {
                return Err(tool_error(format!("`tasks` is not valid: {task:?}")));
            }
            if let Some(task) = foreground_task(&tasks, config) {
                return Err(tool_error(format!(
                    "task '{task}' is a foreground task: it runs until stopped and takes over \
                     the terminal, so it cannot be run from here. Start it in a shell instead: \
                     `yatr {task}`"
                )));
            }
            let tail = args
                .get("tail")
                .and_then(Value::as_u64)
                .map_or_else(|| AGENT_TAIL.to_string(), |n| n.to_string());
            let mut command_line = own(&["run", "--json", "--tail", &tail]);
            if flag("force") {
                command_line.push("--force".into());
            }
            if flag("keep_going") {
                command_line.push("--keep-going".into());
            }
            if args.get("affected").is_some_and(|a| !a.is_null()) {
                command_line.push(format!("--affected={}", string_arg(args, "affected")?));
            }
            command_line.extend(tasks.into_iter().map(String::from));
            Ok(command_line)
        }
        other => Err(tool_error(format!("unknown tool: {other}"))),
    }
}

/// The first of `tasks`, or of what they depend on, that is a foreground
/// task. A config that does not load is left for the child to report.
fn foreground_task(tasks: &[&str], config: Option<&Path>) -> Option<String> {
    let (config, _) = Config::load(config).ok()?;
    let graph = TaskGraph::from_config(&config).ok()?;
    graph
        .execution_order_all(tasks)
        .ok()?
        .into_iter()
        .find(|t| t.config.foreground)
        .map(|t| t.name.clone())
}

/// Ask a child to stop: SIGTERM where there is one, so a `yatr run` can stop
/// its own tasks first.
fn terminate(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
        return;
    }
    let _ = child.start_kill();
}

async fn read_all(stream: Option<impl tokio::io::AsyncRead + Unpin>) -> String {
    let mut bytes = Vec::new();
    if let Some(mut stream) = stream {
        let _ = stream.read_to_end(&mut bytes).await;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Run one tool call. `None` when it was cancelled, which gets no response.
async fn call(
    exe: &Path,
    config: Option<&Path>,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Option<Value> {
    let command_line = match arguments(name, args, config) {
        Ok(command_line) => command_line,
        Err(result) => return Some(result),
    };

    // The subcommand goes first: an option ahead of it would make it a task name.
    let mut command = tokio::process::Command::new(exe);
    command.arg(&command_line[0]);
    if let Some(config) = config {
        command.arg("--config").arg(config);
    }
    command
        .args(&command_line[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Some(tool_error(format!("could not start yatr: {e}"))),
    };
    let stdout = tokio::spawn(read_all(child.stdout.take()));
    let stderr = tokio::spawn(read_all(child.stderr.take()));

    let status = tokio::select! {
        status = child.wait() => status,
        () = cancel.cancelled() => {
            terminate(&mut child);
            let _ = child.wait().await;
            return None;
        }
    };
    let ok = status.is_ok_and(|s| s.success());
    let stdout = stdout.await.unwrap_or_default();
    let stderr = stderr.await.unwrap_or_default();

    let Ok(mut doc) = serde_json::from_str::<Value>(&stdout) else {
        // `check` prints text, as does anything that failed before its JSON.
        let text = [stdout.trim_end(), stderr.trim_end()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return Some(tool_result(&text, None, !ok));
    };
    if name == "run" {
        // What a passing task printed is in its log; only a failure's is news.
        for task in doc["tasks"].as_array_mut().into_iter().flatten() {
            if task["success"] == true {
                if let Some(task) = task.as_object_mut() {
                    task.remove("output");
                }
            }
        }
    }
    let text = serde_json::to_string(&doc).unwrap_or_default();
    Some(tool_result(&text, Some(doc), !ok))
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = PROTOCOLS
        .iter()
        .find(|v| **v == asked)
        .or(PROTOCOLS.last())
        .copied()
        .unwrap_or_default();
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "yatr", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// Serve until stdin closes.
pub async fn serve(config: Option<PathBuf>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let out: Out = Arc::new(Mutex::new(tokio::io::stdout()));
    let running: Running = Arc::default();
    let mut calls = JoinSet::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            send(&out, &failure(&Value::Null, -32700, "Parse error")).await;
            continue;
        };
        let params = &message["params"];
        let (Some(method), id) = (message["method"].as_str(), message.get("id")) else {
            continue;
        };
        let Some(id) = id else {
            if method == "notifications/cancelled" {
                let key = params["requestId"].to_string();
                if let Some(token) = running.lock().ok().and_then(|r| r.get(&key).cloned()) {
                    token.cancel();
                }
            }
            continue;
        };

        match method {
            "initialize" => send(&out, &response(id, initialize(params))).await,
            "ping" => send(&out, &response(id, json!({}))).await,
            "tools/list" => send(&out, &response(id, json!({ "tools": tools() }))).await,
            "tools/call" => {
                let Some(name) = params["name"].as_str().map(String::from) else {
                    send(&out, &failure(id, -32602, "tools/call needs a tool name")).await;
                    continue;
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let cancel = CancellationToken::new();
                if let Ok(mut running) = running.lock() {
                    running.insert(id.to_string(), cancel.clone());
                }
                let (id, exe, config) = (id.clone(), exe.clone(), config.clone());
                let (out, running) = (Arc::clone(&out), Arc::clone(&running));
                calls.spawn(async move {
                    let result = call(&exe, config.as_deref(), &name, &args, cancel).await;
                    if let Ok(mut running) = running.lock() {
                        running.remove(&id.to_string());
                    }
                    if let Some(result) = result {
                        send(&out, &response(&id, result)).await;
                    }
                });
            }
            _ => send(&out, &failure(id, -32601, "Method not found")).await,
        }
    }

    // The client has gone: stop whatever it left running.
    for token in running
        .lock()
        .map(|r| r.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default()
    {
        token.cancel();
    }
    while calls.join_next().await.is_some() {}
    Ok(())
}
