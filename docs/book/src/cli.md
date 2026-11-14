# CLI reference

```text
yatr <COMMAND>

Commands:
  run       Run one or more tasks
  list      List available tasks
  watch     Watch for changes and re-run
  graph     Show the task dependency graph
  affected  List tasks affected by changes since a git ref
  cache     Manage the task cache
  init      Create a yatr.toml template
  check     Validate yatr.toml (referenced files, config smells)
  schema    Print the JSON Schema for yatr.toml
  lsp       Run the yatr.toml language server (LSP over stdio)
  explain   Say whether a task would run or come from cache, and why
  agents    Print or update the AGENTS.md section describing the tasks
  mcp       Run a Model Context Protocol server over stdio
```

`explain`, `agents` and `mcp` were added after 1.0. A task with one of those
names is still what the bare form runs (`yatr explain` runs the task), so adding
them broke nobody; rename the task to reach the subcommand. They are described
in [Coding agents](./agents.md).

## `run`

```bash
yatr run [TASKS]... [OPTIONS]
  --dry-run            Show the execution plan without running
  --force              Ignore the cache
  --parallel <N>       Limit parallelism (0 = `[settings] parallelism`, else CPU count)
  --shell              Use a shell to execute commands
  --json               Structured JSON output instead of human output
  --profile <PATH>     Write a Chrome trace of the run
  --affected <GIT_REF> Only run tasks affected by changes since the ref
  --trace-io           Warn when a task writes outside its declared `outputs`
  -k, --keep-going     After a failure, let tasks that do not depend on it finish
  --agent              Report for a coding agent (also `YATR_AGENT=1`)
  --ndjson             One JSON event per line as the run progresses
  --tail <LINES>       Keep the last N lines of each task's output in the
                       agent and JSON reports; the rest goes to .yatr/logs/
```

The bare form `yatr [OPTIONS] [TASKS]...` is shorthand for `yatr run` and takes
the same options, before or after the task names. Every name is checked before
anything runs, and several names are planned as one graph, so a shared
dependency runs once.

The `--dry-run` plan lists each task by stage with what it would run, its `cwd`
and `env`, and a `mode:` line naming whichever of `shell`, `parallel`,
`no_cache` and `foreground` apply.

## Global options

```bash
  -c, --config <PATH>  Config file path
  -v, --verbose        Print each command before it runs
  -q, --quiet          Only task output and errors: no status lines or summary
      --cwd <DIR>      Working directory
      --no-color       Disable colours
```

Global options may come before the subcommand or after it: `yatr -c ci.toml list`
and `yatr list -c ci.toml` are the same.

`-v` prints each command, as `$ <command>`, on stderr just before it runs.
`-q` drops the per-task status lines, the spinner and the closing summary; task
output, a failed task's `✗` line and its error are still printed. The two cannot
be combined.

## Examples

```bash
yatr graph --format dot build | dot -Tpng > graph.png
yatr list --format json
yatr watch --clear test
yatr cache stats
```
