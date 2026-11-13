# Defining tasks

A task is a `[tasks.<name>]` table. It runs shell commands, a Rhai script, or a
WASM plugin — exactly one of `run` / `script` / `wasm`.

```toml
[tasks.fmt]
desc = "Format and lint"
run = ["cargo fmt", "cargo clippy --fix --allow-dirty"]
```

Subcommand names (`run`, `list`, `watch`, `graph`, `cache`, `init`, `check`,
`schema`, `lsp`, `affected`, `help`) win over a task of the same name in the bare
form: `yatr check` validates the config, and only `yatr run check` reaches the
task. `yatr check` warns about any task named like this.

## Dependencies

```toml
[tasks.check]
desc = "Full check pipeline"
depends = ["fmt", "lint", "test"]   # run these first
```

Dependencies form a DAG; yatr runs each task as soon as its dependencies finish
(a [ready-queue scheduler](./benchmarks.md)), bounded by `--parallel`.

A task with only `depends` and no `run`, `script` or `wasm` is an aggregate. It
has nothing to run, so it is never cached, gets no status line and is not counted
in the summary.

## Parallel commands

```toml
[tasks.lint]
parallel = true
run = ["cargo fmt --check", "cargo clippy -- -D warnings", "cargo doc --no-deps"]
```

## Environment, working dir, shell

```toml
[env]                       # global
RUST_LOG = "debug"

[tasks.migrate]
cwd = "./backend"
env = { DATABASE_URL = "postgres://localhost/dev" }
shell = true
run = ["diesel migration run"]
```

A task runs in the directory of the file that declares it, or in `cwd` resolved
against that directory. `sources`, `outputs`, `watch` and `wasm` are relative to
the same place, so running `yatr` from a subdirectory changes nothing, and a task
in an included file sees paths relative to that file.

## Long-running processes

```toml
[tasks.dev]
foreground = true           # inherit stdio; not cached
run = ["cargo watch -x run"]
```

## Output

A task's stdout and stderr are streamed as it runs, each line prefixed with the
task's name so parallel tasks stay readable. stderr stays on stderr, and is
shown for tasks that succeed as well as for those that fail. A cache hit replays
the output captured when the task last ran (all of it on stdout, since the two
streams are stored together). With `--json` nothing is streamed; each task's
output is in the `output` field of the document instead.

If yatr's stdout is closed early (`yatr test | head`), further output is dropped
and the run carries on; the exit status still reports how the tasks went.

## Failure, cancellation and timeouts

Each `run` command gets its own process group. When a task fails, yatr stops the
tasks still running: their whole process group gets SIGTERM, then SIGKILL after
three seconds, and they are reported as cancelled. Tasks that had not started are
not started. With `--keep-going`, tasks that do not depend on the failed one run
to completion instead; its dependents are still skipped. yatr exits with the
failing command's exit code.

SIGINT and SIGTERM sent to yatr are forwarded the same way, and yatr then exits
with 128 plus the signal number. A `foreground` task stays in yatr's process
group so that it keeps the terminal.

`timeout = <seconds>` bounds the whole task, across all of its commands. A task
over time is killed as above and fails with exit code 124. It applies to `run`
commands only; a `script` or `wasm` task is not interrupted.

## Full task reference

| Field | Meaning |
|-------|---------|
| `desc` | Human description |
| `run` / `script` / `wasm` | What to execute (mutually exclusive) |
| `depends` | Tasks to run first |
| `parallel` | Run `run` commands concurrently |
| `env`, `cwd`, `shell` | Environment, working dir, shell mode |
| `env_inputs` | Ambient variables that are [cache](./caching.md) inputs |
| `foreground` | Inherit stdio (dev servers); not cached |
| `sources`, `outputs` | [Caching](./caching.md) inputs/outputs |
| `watch` | File patterns for `yatr watch` |
| `no_cache`, `allow_failure`, `timeout` | Per-task behaviour |
