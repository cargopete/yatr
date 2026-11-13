# Defining tasks

A task is a `[tasks.<name>]` table. It runs shell commands, a Rhai script, or a
WASM plugin — exactly one of `run` / `script` / `wasm`.

```toml
[tasks.fmt]
desc = "Format and lint"
run = ["cargo fmt", "cargo clippy --fix --allow-dirty"]
```

## Dependencies

```toml
[tasks.check]
desc = "Full check pipeline"
depends = ["fmt", "lint", "test"]   # run these first
```

Dependencies form a DAG; yatr runs each task as soon as its dependencies finish
(a [ready-queue scheduler](./benchmarks.md)), bounded by `--parallel`.

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

## Long-running processes

```toml
[tasks.dev]
foreground = true           # inherit stdio; not cached
run = ["cargo watch -x run"]
```

## Full task reference

| Field | Meaning |
|-------|---------|
| `desc` | Human description |
| `run` / `script` / `wasm` | What to execute (mutually exclusive) |
| `depends` | Tasks to run first |
| `parallel` | Run `run` commands concurrently |
| `env`, `cwd`, `shell` | Environment, working dir, shell mode |
| `foreground` | Inherit stdio (dev servers); not cached |
| `sources`, `outputs` | [Caching](./caching.md) inputs/outputs |
| `watch` | File patterns for `yatr watch` |
| `no_cache`, `allow_failure`, `timeout` | Per-task behaviour |

## Watch mode

`yatr watch <task>` runs the task once, then again whenever a watched file
changes. Each task in the chain (the task and its dependencies) contributes its
`watch` patterns, or its `sources` if it has no `watch`, matched relative to that
task's `cwd`. If none declares either, `**/*.rs`, `**/*.toml` and `Cargo.lock`
are watched. `*` does not cross `/`; use `**` for that. Pinned `[toolchain]`s are
put on `PATH` as for `yatr run`. The cache is off in watch mode, so every change
re-runs the whole chain. `--clear` clears the screen before each run.
