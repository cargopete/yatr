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

- Publish a **JSON Schema** for `yatr.toml`; document editor setup (VS Code / JetBrains).
- Polish `check` and error messages toward best-in-class.
- **Structured JSON run output** + per-task timing in the run summary.
- `--profile` producing a Chrome-trace / flamegraph.

### v0.4 — Remote cache (the headline) 🟢 strategic differentiator #1

On top of the now-real local CAS:

- **Bazel Remote Execution API, HTTP variant** (`/ac/` action cache + `/cas/` blob store) with
  PUT / GET / HEAD, async writes, and transparent local→remote failover (Turborepo's model).
- Interoperate with off-the-shelf servers (bazel-remote first; BuildBuddy / NativeLink / Depot later).
- **HMAC artifact signing** + immutable entries + scoped read/write tokens to prevent cache
  poisoning. (Heed Nx CVE-2025-36852 "CREEP" as the cautionary tale.)
- Use **SHA-256** keys for ecosystem interop here (BLAKE3 stays the fast local default).

### v0.5+ — Scale & extensibility

- **`--affected` monorepo mode:** git-diff + project-graph reachability so large repos only run
  impacted tasks. Per-directory `yatr.toml` includes / inheritance.
- **WASM / Extism plugin system** 🟢 strategic differentiator #2 — the clearest moat. A
  capability-sandboxed, polyglot plugin API (custom task types, cache-key contributors,
  toolchain providers, output reporters), plugins distributed as `.wasm` via file/GitHub
  locators. Leans directly on the author's WASM/WASI/Wasmtime expertise; the simple-runner tier
  has nothing like it.
- **Toolchain management / pinning** — the single biggest _polyglot_ gap. A `[toolchain]`
  section that pins + auto-downloads language runtimes, or deep integration with mise / proto.
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
