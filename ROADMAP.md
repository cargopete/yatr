# yatr Roadmap

> Status: draft for review. Reflects the codebase as of v0.1.2 (June 2026).

## Positioning

yatr's niche is **the correct, fast, hermetic polyglot _command runner_** — not a build
system. The goal is to beat Just / Task / mise on caching correctness and reproducibility
while staying an order of magnitude simpler than Bazel / Buck2.

We deliberately do **not** chase remote _execution_ (shipping actions to worker pools) until
local caching, affected-detection, and toolchain management are excellent. Revisit only if
real users consistently hit CI-scale builds that local + remote _caching_ can't satisfy.

The three capabilities that separate world-class polyglot runners from merely good ones:

1. **Cross-machine (remote) caching**
2. **Hermetic / sandboxed execution**
3. **Monorepo-aware change detection**

yatr has the right architectural DNA for all three; it has shipped none of them yet.

## Where we actually stand (ground truth, not aspiration)

**Already real and good:**

- Rust single binary, zero runtime deps — the cross-platform + startup-time moat. Keep inviolable.
- DAG scheduler with dependency resolution and parallel execution (`petgraph`).
- Shell-less-by-default execution — a genuine correctness/portability edge over Just and Make.
- **BLAKE3** content hashing is already in use (`cache.rs`). The "switch to BLAKE3" item is done.
- Rhai for inline task logic; `--dry-run`; `graph --format dot`; `check`; debounced watch mode.

**The load-bearing gap most external analysis gets wrong:** the local cache is **not yet a
real artifact cache**. Today it:

- Hashes task name + `run`/`script` + env + `sources` into the key — but **not `outputs`**.
- Stores and replays **stdout only**; it does not capture or restore built files.
- Does **not** cache exit status, and does **not** validate that declared `outputs` still exist
  on a cache hit. (Delete `target/`, get a hit, end up with no binary.)
- `outputs` is parsed in `config.rs` and read nowhere.
- `hash_sources` walks the whole tree from `.` per task, follows symlinks, and ignores
  `.gitignore` — O(repo × tasks) and a symlink-loop hazard.

**Consequence for the roadmap:** remote caching (REAPI `/ac/` + `/cas/`) needs a local
content-addressed artifact store to share. We don't have one — we have a stdout memoiser.
So **finishing the local cache is the prerequisite for the headline feature**, not a parallel
nicety. Once the local CAS is real, remote becomes a backend swap, exactly as the
moon v1.30 (gRPC) → v1.32 (HTTP + Depot) precedent suggests.

## Milestones

### v0.2 — "Make the cache true" (foundation) 🔴 highest priority

The piston the rest of the engine needs.

- [x] Include `outputs` declarations in the cache key (also `cwd` and shell mode).
- [x] On success, capture declared output files into a local content-addressed store (CAS).
  The cache is now split into `ac/` (ActionResult JSON) + `cas/` (BLAKE3 blobs), mirroring
  the Bazel REAPI shapes so v0.4's remote backend is a swap behind the same async API.
- [x] On a cache hit, **restore** outputs from the CAS; if any blob is missing, fall through
  to a real run rather than report a false hit.
- [x] Record **exit status** and duration in the ActionResult (only successful runs are cached;
  failures re-run, as they should). Foreground tasks are excluded from caching.
- [x] Fix `hash_sources`: respect `.gitignore` (via the `ignore` crate), root the walk at the
  task's `cwd`, and stop following symlinks.
- [x] Resolve the `duration_ms: 0` TODO and implement `yatr cache clear <task>` (`main.rs`).
- [ ] **IO-tracing-lite:** warn when a task reads a file outside `sources` or writes outside
  `outputs`. Deferred — it needs platform-specific syscall tracing (strace/dtrace/ptrace) and is
  large enough to stand alone. Tracked for a v0.2.x follow-up. Cache correctness before cache
  sharing — a fast cache that is occasionally wrong is worse than no cache.

_Acceptance (met):_ `yatr build && rm -rf <outputs> && yatr build` restores outputs from cache
and the artifacts are present; changing a `sources` file busts the key; changing an unrelated
file does not. Verified end-to-end against the binary.

### v0.3 — DX & observability (cheap, compounding, parallelisable)

- [x] Publish a **JSON Schema** for `yatr.toml` (`schemars`-derived; `yatr schema`
  subcommand + committed `yatr.schema.json`); editor setup documented in the README.
- [x] **Structured JSON run output** (`yatr run --json`): per-task results + summary, and a
  JSON execution plan under `--json --dry-run`. Per-task timing is already shown inline.
- [x] `--profile <path>` producing a Chrome trace (one event per task on the run timeline,
  viewable in `chrome://tracing` / Perfetto).
- [x] `check` validates referenced files (wasm plugins, task `cwd`s), warns on config smells
  (`outputs` + `no_cache`, foreground with multiple commands), and exits non-zero on errors.

### v0.4 — Remote cache (the headline) 🟢 strategic differentiator #1

On top of the now-real local CAS. **Slice 1 (yatr-native protocol) shipped:**

- [x] HTTP cache with **`/ac/` + `/cas/` PUT/GET/HEAD** (Bazel's path layout), read-through on a
  local miss and write-through after a run, with **transparent failover** — a flaky or absent
  remote warns and the build continues, never errors.
- [x] **Portable, content-addressed keys** — the cache key hashes the task's *relative* `cwd`
  (not the absolute path), so entries are shareable across machines and CI checkouts. (This was a
  real bug caught by a cross-machine end-to-end test; the unit tests alone missed it.)
- [x] Config surface (`[settings.remote_cache]`: `url`, `token_env`, `read`, `write`) with bearer
  auth sourced from an env var (no secrets in the committed file). Verified end-to-end against a
  filesystem-backed server and with `wiremock` integration tests.

**Slice 2 (integrity & signing) shipped:**

- [x] **CAS blob integrity** — downloaded blobs are verified against their content digest; a
  digest mismatch (tampered/corrupt data) rejects the entry.
- [x] **Action-result signing** — a keyed BLAKE3 MAC over each action result (secret from
  `sign_key_env`), verified constant-time on read; entries that fail are rejected loudly. The
  defence against the Nx CVE-2025-36852 "CREEP" poisoning class. No new crypto dependencies.

**Remaining for v0.4:**

- [ ] **REAPI interop**: SHA-256 digests + protobuf `ActionResult` so the cache plugs into
  off-the-shelf servers (bazel-remote first; BuildBuddy / NativeLink / Depot later). BLAKE3 stays
  the fast local default.
- [ ] Immutable entries + scoped read/write tokens (the rest of the poisoning hardening).

### v0.5+ — Scale & extensibility

- [x] **`affected` monorepo mode:** `yatr affected <git-ref>` lists tasks touched by changes
  since a ref (sources/watch glob match → directly affected, propagated to transitive
  dependents); `yatr run --affected <ref>` filters a run to the affected subset.
- [x] **Config includes** — `include = [...]` composes task definitions across multiple
  `yatr.toml` files (recursive, cycle-detected, duplicate-task errors), so a repo can keep tasks
  next to the code they build.
- **WASM plugin system** 🟢 strategic differentiator #2 — the clearest moat. **Slice 1 shipped:**
  - [x] WASM-backed task type (`wasm = "plugin.wasm"`) running in a pure-Rust `wasmi` interpreter,
    **capability-sandboxed** (only yatr's `emit`/`log` host ABI imported — no fs/network/clock).
    Plugins export `run() -> i32`; output is captured and cached like any task. Also accepts `.wat`.
  - [x] Plugins read their input — `input_len`/`input_read` host fns deliver the task name and
    env as JSON, so a single plugin can be parameterised per task.
  - [ ] Declare cache-key contributions + structured I/O from plugins.
  - [x] Remote plugin locators — `wasm = "https://…/plugin.wasm"` downloads once into a local
    plugin cache (`YATR_PLUGIN_DIR`) and reuses it; remote plugins still run sandboxed.
  - [x] **PDK** (`yatr-plugin` crate) — write plugins in ergonomic Rust (`plugin!` macro +
    `emit`/`log`/`input_string`); host stubs let it build/test off-wasm. Verified end-to-end:
    a PDK plugin compiled to wasm32 and ran in yatr, reading its task env.
  - [x] GitHub-shorthand locators — `github:owner/repo@tag/asset.wasm` resolves to that release
    asset and reuses the remote download/cache path.
  - [ ] More plugin roles: custom task runners, cache-key contributors, toolchain providers,
    output reporters.
- [x] **Toolchain management / pinning** — the single biggest _polyglot_ gap. A `[toolchain]`
  section pins + auto-downloads language runtimes (templated `url` with `{version}/{os}/{arch}`,
  `.tar.gz`) and prepends their `bin` to the task `PATH`. Verified end-to-end. (Remaining: zip
  archives, built-in presets for common tools.)
- **gRPC REAPI cache** with `GetCapabilities` digest negotiation (broader server compatibility).

### v1.0+ — Frontier

- **Graduated hermeticity:** opt-in sandbox (declared inputs only) building on the IO-tracing
  work; reproducibility verification (`SOURCE_DATE_EPOCH`, diff re-runs).
- **Partial / incremental caching** within large tasks — avoid moon's all-or-nothing pain.
- **LSP** for `yatr.toml` (go-to-task, hover docs, diagnostics).
- **Language presets** (cargo / npm / go / uv / docker) shipped as WASM plugins — dogfooding the
  plugin system.
- Optional **remote execution** — only if user demand justifies leaving the runner niche.

## Design principles

- **Keep the core language-agnostic** (Buck2's lesson): expose capabilities to extensions rather
  than privileging built-ins.
- **Single binary stays small**: distribute extensions as external `.wasm`, not baked in.
- **Rhai for inline logic, WASM for distributable plugins.** Rhai is a thin dynamic wrapper over
  Rust, not an application language — scope it accordingly, and consider a "pure" mode that
  constrains its filesystem/exec helpers for reproducible evaluation (Starlark's determinism as
  the inspiration, not the implementation).

## Caveats

- This roadmap is an architecture-and-capabilities plan, not an adoption plan. yatr is
  early-stage (v0.1.2).
- Competitor specifics evolve quickly; versions cited are snapshots as of June 2026 (moon v1.32
  HTTP remote cache, Feb 2026; Nx self-hosted cache deprecation, May 2026; Earthly in maintenance
  mode; Turborepo v2.x).
- Benchmark figures from competitors' own materials (Rhai ~2× slower than Python 3; BLAKE3 vs
  SHA-256; "Buck2 2× faster than Buck1") should be validated against yatr's own workloads before
  driving decisions.
- Adopting REAPI assumes we want ecosystem interoperability. A bespoke protocol would be simpler
  short-term but forgoes the off-the-shelf server ecosystem (bazel-remote, BuildBuddy,
  NativeLink, Depot).
