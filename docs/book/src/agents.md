# Coding agents

A coding agent runs tasks the way a person does, with two differences: it pays
for every line it reads, and it cannot scroll back. yatr has a report written
for that reader, a command that tells the agent the tasks exist, one that
explains the cache, and an MCP server for clients that prefer tools to a shell.

## The agent report

```bash
yatr run --agent ci
```

```text
ok   fmt-check (cached)
ok   lint 12.31s
FAIL test 48.10s: exit 101 from `cargo nextest run --locked`
--- test: last 40 of 1832 lines, full log: .yatr/logs/test.log ---
…the last 40 lines of what the task printed…
--- end test ---
skip features (cancelled)
FAILED: 1 failed, 2 ok (1 cached), 1 cancelled, 2 not run, 60.42s
rerun: yatr run --agent test
```

A task that passes is one line. A task that fails adds the command that failed,
its exit code and the tail of its output. Nothing is thrown away: the whole
output of every task is written to `.yatr/logs/<task>.log`, in a directory that
ignores itself, so it does not appear in `git status`.

- `--tail <LINES>` sets how much of a failure is shown. The default is 40.
- A line longer than 400 characters is clipped, with a count of what was cut.
- `YATR_AGENT=1` in the environment selects the report without the flag, for
  the bare form as well: `YATR_AGENT=1 yatr ci`. `--json` and `--ndjson` given
  by name still win.
- A task with `allow_failure = true` that fails is reported as `warn` and does
  not fail the run.
- The report goes to stdout. The exit code is the failing command's, as ever.

## JSON

`yatr run --json` prints one document when the run ends:

```json
{
  "ok": false,
  "tasks": [
    {
      "name": "test",
      "success": false,
      "cached": false,
      "cache": "miss",
      "duration_ms": 48100,
      "command": "cargo nextest run --locked",
      "exit_code": 101,
      "error": "Command failed with exit code 101: cargo nextest run --locked",
      "output": "…",
      "cancelled": false,
      "allowed_failure": false
    }
  ],
  "summary": { "succeeded": 2, "failed": 1, "cancelled": 1, "cached": 1, "duration_ms": 60420 },
  "error": { "code": "yatr::exec::failed", "message": "Task 'test' failed with exit code 101", "task": "test", "exit_code": 101 }
}
```

`cache` is one of `hit`, `miss`, `forced` (`--force`), `uncacheable` (no
`sources`, `no_cache`, or a foreground task) and `disabled`.

With `--tail <LINES>`, `output` holds only the last lines, and each task gains
`output_lines`, `output_truncated` and `log`, the path of the file holding all
of it. Without `--tail`, `output` is whole and no log is written.

`yatr run --ndjson` prints the same information as it happens, one JSON object
per line: `run_start`, then `task_start` and `task_finish` for each task, then
`run_finish`. A `task_finish` event has the fields of a task above. With `-v`,
each line a task prints is an `output` event as well, carrying `task`, `stream`
and `line`.

### Errors

A command that was asked for JSON reports its errors as JSON, on stdout, so a
reader never has to parse a diagnostic. That covers `run --json`,
`run --ndjson` (as an `error` event) and `--format json` on `list`, `affected`,
`graph` and `explain`:

```json
{
  "error": {
    "code": "yatr::task::not_found",
    "message": "Task 'biuld' not found; did you mean `build`?",
    "help": "Run `yatr list` to see available tasks",
    "task": "biuld",
    "suggestions": ["build"],
    "available": ["build", "test"]
  }
}
```

A run that got as far as running tasks prints its usual document, with `ok`
false and the error under `error`. It is never followed by a second document.
The human-readable diagnostic still goes to stderr.

### Describing the tasks

`yatr list --format json` gives, for each task, its `description`, `depends`,
`kind` (`commands`, `script`, `wasm` or `aggregate`), `run`, `sources`,
`outputs`, and whether it is `cached` or runs in the `foreground`. It is enough
to choose a task without reading the config.

## `yatr explain`

```bash
yatr explain build
```

```text
build: runs - its inputs changed since it last ran (last ran 2026-11-14 10:02 UTC)
  sources: 1 modified, 1 added
    modified src/cache.rs
    added src/explain.rs
  dependencies: 1 changed
    changed gen
gen: runs - its inputs changed since it last ran (last ran 2026-11-14 10:02 UTC)
  sources: 1 modified
    modified schema/api.json
fmt: always runs - `no_cache = true`
```

`explain` says, for a task and everything it depends on, whether a run would
restore it from cache or execute it, and why. Nothing is executed. The verdict
is one of:

- `cached`: its inputs match a cache entry, which a run restores.
- `runs`: there is no usable entry for its current inputs. The inputs that
  differ from its last execution are listed by kind: `sources`,
  `dependencies`, `definition` (its commands, script, `cwd` or shell mode),
  `env`, `env_inputs`, `tool_inputs`, `toolchains` and `outputs`.
- `always runs`: it is never cached, and the reason is given.
- `aggregate`: it has nothing to run of its own.

`--format json` gives the same as a document.

Two limits, stated rather than left to be discovered. The comparison is with
the task's last execution on this machine, not with whatever is in a remote
cache, which only a run consults. And a dependency that is not cached but
declares `outputs` is fingerprinted by what it leaves on disk, which a run only
knows after running it; `explain` uses what is on disk now.

## `yatr agents`

An agent uses the tasks it is told about. `yatr agents` prints a section for
`AGENTS.md`, generated from `yatr.toml`: how to run tasks, the gate to run
before reporting work as done (the `ci` task, where there is one), and a table
of every task with what it does, what it runs after, and whether it is cached,
always runs, or is long-running and must not be waited for.

```bash
yatr agents                    # print the section
yatr agents --write            # write it into AGENTS.md beside yatr.toml
yatr agents --write CLAUDE.md  # or into another file
yatr agents --check            # exit non-zero if the section is missing or stale
```

`--write` replaces the text between `<!-- yatr:begin -->` and
`<!-- yatr:end -->` and leaves the rest of the file alone, appending the section
if the file has none. `--check` is for CI, so the section cannot drift from the
tasks. The section's wording belongs to the yatr version that wrote it, so a
`--check` in CI wants the same yatr as the one that ran `--write`.

## `yatr mcp`

`yatr mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio. Its tools:

| Tool | What it does |
|------|--------------|
| `list` | The tasks, as `yatr list --format json` |
| `run` | Run tasks; takes `tasks`, and optionally `force`, `keep_going`, `affected` and `tail` |
| `explain` | A task's cache verdict, as `yatr explain --format json` |
| `affected` | The tasks affected since a `git_ref` |
| `check` | Validate `yatr.toml` |

`run` returns the `--json` document with each failed task's output cut to its
tail, and the output of passing tasks left out; `log` names the file with all
of it. A foreground task is refused, since it would hold the call open until
somebody stopped it. A cancelled call stops the tasks it started.

To register it with Claude Code:

```bash
claude mcp add yatr -- yatr mcp
```

or in a project's `.mcp.json`:

```json
{ "mcpServers": { "yatr": { "command": "yatr", "args": ["mcp"] } } }
```

An agent with a shell needs none of this: `yatr run --agent` does the same job
in fewer tokens. The server is for clients without one.
