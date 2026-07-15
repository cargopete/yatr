# Changelog

All notable changes to **yatr** are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and from 1.0.0 onward
yatr adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
