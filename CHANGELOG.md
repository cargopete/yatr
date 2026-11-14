# Changelog

All notable changes to **yatr** are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and from 1.0.0 onward
yatr adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- A shell task that leaves a process in the background (`server & run-tests`) ends when its command exits, rather than when that process does. yatr reads what was already written, stops reading, and warns if the background process still holds the task's output (#54).

## [1.4.0] - 2026-11-14

yatr for coding agents: a report that fits a context window, a command that explains the cache, one that tells an agent the tasks exist, and an MCP server. See **Fixed** if a task of yours depends on an aggregate.

### Added

- `yatr run --agent` reports for a coding agent: one line per task, and for a failure the command that failed, its exit code and the last 40 lines of its output. `YATR_AGENT=1` selects it without the flag. The whole output of every task goes to `.yatr/logs/<task>.log`, a directory that ignores itself.
- `--tail <LINES>` keeps the last lines of each task's output in `--agent`, `--json` and `--ndjson`. In JSON each task gains `output_lines`, `output_truncated` and `log`.
- `yatr run --ndjson` streams the run as one JSON event per line: `run_start`, `task_start`, `task_finish`, `run_finish`, and with `-v` an `output` event per line a task prints.
- A command asked for JSON reports its errors as JSON on stdout: `run --json`, `run --ndjson`, and `--format json` on `list`, `affected` and `graph`. A task that is not found carries `suggestions` and `available`, and the message names the near miss.
- `yatr explain <task>` says whether a task and its dependencies would run or be restored from cache, and which inputs changed since each last ran, by kind and by name. `--format json` for the same as a document.
- `yatr agents` prints the `AGENTS.md` section describing the project's tasks; `--write [FILE]` writes it in place between its markers, `--check [FILE]` fails when it is missing or stale.
- `yatr mcp` runs a Model Context Protocol server over stdio, with `list`, `run`, `explain`, `affected` and `check` as tools.
- The `run --json` document gains `ok` and, when the run failed, `error`; each task gains `cache` (`hit`, `miss`, `forced`, `uncacheable`, `disabled`), `command` and `allowed_failure`.
- `yatr list --format json` gives each task's `depends`, `kind`, `run`, `sources`, `outputs`, `cached` and `foreground`.

### Changed

- A run flag ahead of `run` now applies to it: `yatr --dry-run run deploy` is a dry run, where it was an error.
- A task named `explain`, `agents` or `mcp` is still what `yatr <name>` runs. The new subcommands yield to it, and `yatr check` says nothing about the name.

### Fixed

- An option ahead of a subcommand no longer turns the subcommand into a task name: `yatr -c ci.toml list` and `yatr -q run test` failed with "Task 'list' not found". Where a task does have that name, it is still what runs.
- A task that depends on an aggregate (`run = []`) now re-runs when something behind the aggregate changes. Its key held only the aggregate's name, so it replayed a stale result. Such tasks miss once after upgrading.

## [1.3.0] - 2026-11-13

Status and summary lines move to stderr; see **Changed** if anything reads yatr's stdout.

### Added

- A `tool_inputs` task key names commands, such as `nuthatch --version`, whose stdout is a cache input, so upgrading a tool re-runs the tasks that use it. Each command runs once per invocation (#46).
- `env` values expand `${VAR:-default}` and `${VAR-default}`, so `[env]` can give a default the caller overrides; `yatr check` warns on any other `${...}` form, which is passed through literally (#45).

### Changed

- `yatr watch` prints its banner and separators to stderr, like the run status lines (#44).
- **Status and summary lines now go to stderr, not stdout**, and a run of a single task prints its output without the `task │` prefix. stdout carries only what tasks print, so `yatr gen > out.txt` captures the output alone. Anything that parsed yatr's stdout for `✓`, `✗` or the summary must read stderr instead, or use `--json` (#44).

### Fixed

- `-q` now suppresses status lines, the spinner and the summary while keeping task output and failures, and `-v` prints each command before it runs; the two conflict. Both were accepted and ignored before (#43).
- With `--keep-going`, the closing error lists every failed task with its exit code; when more than one failed yatr exits 1, and a single failure still exits with its own code. The per-command message no longer nests quotes (`Command failed with exit code 3: sh -c 'exit 3'`) (#42).
- `yatr check` says "1 task", and the `--dry-run` plan shows a `mode:` line for `shell` (including `--shell`), `parallel`, `no_cache` and `foreground` (#48).
- Task output lines starting with `::` are printed without the task-name prefix, so GitHub Actions workflow commands and log groups work again (#49).
- A run whose `outputs` pattern captures no files warns, naming the pattern, rather than caching an entry that restores nothing in silence (#41).
- Source hashing walks only each pattern's literal base, not the whole task directory, so `sources = ["src/**"]` beside 40k unrelated files hits in 5 ms rather than 80 ms (#47).

## [1.2.0] - 2026-11-13

Several fixes change behaviour; see **Changed** before upgrading.

### Changed

- Tasks without `sources` are no longer cached (#7).
- `*` in `sources` no longer matches across `/`; use `**` for that (#11).
- `env` values expand `$VAR` and `${VAR}`; write `$$` for a literal `$` (#24).
- Task output streams live, prefixed with the task name (#22).
- `yatr cache clear` clears only the current project; `--all` clears everything (#38).
- Cache entries from 1.1.x miss once and are rebuilt.

### Fixed

- Tasks without `sources` are no longer cached, and local cache entries are scoped to the project directory, so a key no longer collides across projects (#7).
- The cache key covers the global `[env]` and pinned toolchain versions, and a new `env_inputs` task key names ambient variables that are cache inputs (#8).
- A task's cache key includes its dependencies' keys, so a dependent re-runs when a dependency's inputs change (#9).
- `sources`, `outputs` and `watch` share one glob dialect: `*` no longer crosses `/` in sources, `outputs = ["dir/**"]` captures the tree, and a leading `./` is ignored (#11).
- A source named inside a gitignored directory is hashed rather than reported missing, and `.gitignore` is honoured outside a git checkout (#12).
- A failing task's stdout and stderr are shown, and yatr exits with the failing command's exit code instead of always 1 (#5).
- Task output is streamed as it runs, prefixed with the task name; stderr is kept on success, leading whitespace is preserved, and a closed stdout no longer panics (#22).
- A failing task's running siblings are killed by process group and reported cancelled, SIGINT and SIGTERM are forwarded to task processes, and `--keep-going` lets independent tasks finish (#18).
- The task `timeout` field is enforced for `run` commands; a task over time is killed and exits 124 (#16).
- `[settings] shell` and `[settings] parallelism` are honoured; `--parallel` still overrides the latter (#17).
- Reporting: the summary gives wall time and says "1 task", `graph` and `--dry-run` print in a stable order with dependencies as declared, the dry-run plan names each task with its `cwd`, `env` and indented multi-line commands, and a `run = []` aggregate is no longer counted or shown as cached (#23).
- Run options after a bare task name (`yatr deploy --dry-run`) are now options, not task names, and every requested name is validated before anything runs (#13).
- `yatr run a b` plans one graph across all named tasks: shared dependencies run once, independent tasks run in parallel, one summary (#14).
- The bare form now accepts the `run` options (`yatr --dry-run build`); README and book no longer document the non-existent `yatr --watch` and `yatr --shell run`, and the `cache_dir` doc names the real default (#15).
- `yatr check` warns about tasks named after a builtin subcommand (including `help`), which the bare form cannot reach, and names `yatr run <name>` (#4).
- `yatr init` writes `yatr.toml`, the name yatr searches for, instead of `YATR.toml`; the template links the real docs and names its aggregate task `ci` rather than the shadowed `check` (#3).
- Task paths (`cwd`, `sources`, `outputs`, `wasm`, affected matching) and a relative `cache_dir` resolve against the directory of the file that declares them, not the shell's, so `yatr` behaves the same from a subdirectory and included files see their own paths; a missing task `cwd` now names the path (#19).
- `yatr watch`: patterns are matched relative to the task's `cwd` (relative patterns never fired), dependencies' patterns are watched, `[toolchain]` pins are applied, and `--clear` clears the screen (#20).
- `yatr affected` diffs from the ref's merge-base to the working tree, counts untracked files, treats a task whose definition changed in `yatr.toml` as affected, matches `sources` against the task's `cwd`, and no longer treats aggregates as always affected; `run --affected` also skips unaffected dependencies (#21).
- `yatr cache clear` clears the current project's entries rather than every project's; `--all` clears everything (#38).
- `[toolchain]` bin dirs are prepended to a `PATH` set in `[env]` instead of replacing it, and stay out of the cache key (#39).

### Added

- `env` values expand `$VAR` and `${VAR}` (`$$` for a literal `$`), so a task can extend `PATH` (#24).
- `yatr check` warns on shell syntax in a command without `shell = true`, and on a cached task with no `sources` (#24).

## [1.1.1] - 2026-11-13

### Fixed

- Cached outputs keep their executable bit and symlinks on restore, and a hit no longer strips the mode of an output already on disk (#10).
- Dependency lockfile updated to clear 16 RUSTSEC advisories.

## [1.1.0] — 2026-07-15

### Added

- **Toolchain archive verification** — a `[toolchain]` entry now accepts an
  optional `sha256`. When set, the downloaded archive is verified against the
  digest **before extraction**; a mismatch aborts the install and leaves nothing
  on disk (no install marker), so a re-tagged URL or a tampered mirror can't reach
  your filesystem. Existing configs without a digest are unaffected. This is a
  backward-compatible new key, per the 1.0 stability promise.

### Changed

- Documented the toolchain fetch failure model (bad URL / 404 / unsupported
  archive → hard failure, never a silent fallback to the system `PATH`).
- Fairer comparison table: cargo-make is now ⚠️ (not ❌) on toolchain management
  and ✅ on watch mode, with footnotes explaining the distinctions.

## [1.0.0] — 2026-06-11

First stable release. The engine has been feature-complete across caching,
remote/shared cache, monorepo affected-detection, toolchain management, sandboxed
WASM plugins, and editor tooling for several releases; 1.0.0 marks the point where
the user-facing surface is **frozen under semver** rather than the point where
features arrive.

### Stability promise

Stable under semver from this release (breaking changes require a 2.0):

- **`yatr.toml` configuration schema** — task fields (`run`, `script`, `wasm`,
  `deps`, `cwd`, `shell`, `sources`, `outputs`, …), `[settings]`,
  `[settings.remote_cache]`, `[toolchain]`, and `include`. New keys may be added
  in minor releases; existing keys keep their meaning.
- **CLI surface** — commands (`run`, `affected`, `check`, `schema`, `cache`,
  `lsp`) and their flags (`--watch`, `--json`, `--profile`, `--trace-io`,
  `--affected`, `--force`, `--dry-run`, …).
- **Cache & remote protocols** — the local content-addressed store layout and the
  HTTP (`/ac/` + `/cas/`) and REAPI (`protocol = "reapi"`) wire formats are stable
  enough that caches survive across 1.x upgrades.

Explicitly **not** yet covered by the 1.0 promise (still pre-1.0, may change):

- The `yatr` **library/crate API** (using yatr as a Rust dependency).
- The **WASM plugin ABI** and the `yatr-plugin` PDK crate (remains `0.x` while the
  plugin roles, cache-key contributions, and structured I/O are still evolving).

### Included since the 0.x line

Everything that shipped across 0.2–0.7 is part of 1.0:

- **Content-addressed caching** with output capture & restore (`ac/` + `cas/`),
  gitignore-scoped source hashing, and `yatr cache clear <task>`.
- **Remote / shared cache** over HTTP with read/write-through, portable keys, and
  failover; **action-result signing** (keyed BLAKE3 MAC) and CAS blob integrity
  checks; **REAPI interop** (SHA-256 + protobuf `ActionResult`) for `bazel-remote`
  / `BuildBuddy`.
- **Monorepo support** — `yatr affected <git-ref>` and `run --affected`, plus
  `include = [...]` config composition.
- **Toolchain pinning** — `[toolchain]` auto-downloads pinned language runtimes
  and prepends them to the task `PATH`.
- **Sandboxed WASM plugins** — capability-sandboxed `wasmi` task type, remote and
  `github:` plugin locators, and the `yatr-plugin` PDK for authoring plugins in
  Rust.
- **Scheduler** — a ready-queue that starts each task the moment its dependencies
  finish (~1.8× faster than the old level-barrier on uneven DAGs).
- **Developer experience** — JSON Schema for `yatr.toml` (`yatr schema`),
  structured `run --json`, `--profile` Chrome traces, `--trace-io` undeclared-write
  warnings, an mdbook documentation site, and a `yatr lsp` language server with
  live diagnostics and a task outline.

### Fixed

- Caching is no longer silently disabled when `yatr.toml` has no `[settings]`
  section.
- Errors render through `miette` (message + help) instead of a `Debug` dump;
  `affected` failures get their own error variant.
- IO-tracing path matching normalised on Windows.

## [0.7.0] — 2026

- REAPI cache interop (`protocol = "reapi"`): SHA-256 digests + protobuf
  `ActionResult`, compatible with `bazel-remote` / `BuildBuddy`.
- mdbook documentation site deployed to GitHub Pages.
- `yatr lsp` language server: live diagnostics + task outline.
- Ready-queue scheduler replacing the level-barrier scheduler.
- IO-tracing (`--trace-io`) and a reproducible benchmark suite.

## [0.6.0] — 2026

- Sandboxed WASM plugins (`wasmi`), remote and `github:` plugin locators, and the
  `yatr-plugin` PDK crate.
- Toolchain pinning (`[toolchain]`).
- `check` polish: validate referenced files, warn on config smells.

## [0.5.0] — 2026

- Monorepo milestone: `affected` change detection, config `include`, and
  `run --profile` Chrome traces.

## [0.4.0] — 2026

- Shared HTTP remote cache (read/write-through, portable keys, failover).
- Action-result signing (keyed BLAKE3 MAC) + CAS blob integrity checks.

## [0.3.0] — 2026

- JSON Schema for `yatr.toml` (`yatr schema`) and structured `run --json` output.

## [0.2.0] — 2026

- Outputs-aware artifact cache (`ac/` + `cas/`), gitignore-scoped source hashing,
  `cache clear <task>`.

## [0.1.0] — 2026

- Initial release: TOML task config, Rhai scripting, dependency resolution,
  parallel execution, file watching.

[1.1.0]: https://github.com/cargopete/yatr/releases/tag/v1.1.0
[1.0.0]: https://github.com/cargopete/yatr/releases/tag/v1.0.0
