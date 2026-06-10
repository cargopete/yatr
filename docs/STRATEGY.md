# yatr — Path to Best-in-Class

> A strategic companion to [ROADMAP.md](../ROADMAP.md). ROADMAP tracks *features by
> milestone*; this tracks *what it takes to be the best in the field, and why*.
> Snapshot as of June 2026 — competitor specifics evolve.

## 1. The field, and what "best" means

**Our field:** the **correct, fast, hermetic polyglot _command runner_**. We beat
Just / Task / mise on caching and reproducibility, and stay an order of magnitude
simpler than Bazel / Buck2. We do **not** chase remote *execution* — that's a
different tier, and leaving it alone is a feature, not a gap.

**"Best" is not one axis.** No single feature wins it. Best-in-class = the product
of four things, and we grade ourselves on all of them:

| Dimension | What it means | Where we stand |
|-----------|---------------|----------------|
| **Trusted** | correct cache, signed artifacts, reproducible | strong — signing ✅, output-restore ✅; hermeticity partial |
| **Fast** | cold-cache speed, incrementality, affected | strong — affected ✅, BLAKE3 ✅; but *unproven* (no benchmarks) |
| **Polyglot-complete** | any language, no "works on my machine" | **gap** — no toolchain management |
| **Delightful** | install, autocomplete, errors, docs | partial — schema ✅; no LSP, no docs site, not yet on crates.io |

The strategic insight: we have **out-built** our adoption. The engine is
best-in-tier; the *proof* and the *polish* lag. So the campaign is two-pronged —
**close the one real feature gap (toolchain)** and **prove + polish what already
exists**.

## 2. Competitive scorecard

Legend: ✅ strong · ⚠️ partial/weak · ❌ absent.

| Capability | Just | Task | mise | moon | Turborepo | **yatr** |
|---|---|---|---|---|---|---|
| Single binary, zero-runtime-deps | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Content-addressed caching | ❌ | ⚠️ | ⚠️ | ✅ | ✅ | ✅ |
| Captures & restores outputs | ❌ | ❌ | ❌ | ✅ | ✅ | ✅ |
| Remote / shared cache | ❌ | ❌ | ❌ | ✅ | ✅ | ✅ |
| Signed cache (anti-poisoning) | ❌ | ❌ | ❌ | ⚠️ | ✅ | ✅ |
| Affected / change detection | ❌ | ❌ | ⚠️ | ✅ | ✅ | ✅ |
| Sandboxed plugins | ❌ | ❌ | ⚠️ | ✅ | ❌ | ✅ |
| Config schema | ⚠️ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ |
| Structured/JSON + profiling output | ⚠️ | ⚠️ | ❌ | ⚠️ | ✅ | ✅ |
| **Toolchain management** | ❌ | ❌ | ✅✅ | ✅ | ❌ | **❌** |
| **LSP / editor intelligence** | ⚠️ | ⚠️ | ❌ | ⚠️ | ✅ | **❌** |
| **Hermeticity / sandboxed tasks** | ❌ | ❌ | ❌ | ⚠️ | ❌ | **⚠️** |
| **REAPI ecosystem interop** | ❌ | ❌ | ❌ | ✅ | ❌ | **❌** |
| **Adoption: published, docs, benchmarks** | ✅ | ✅ | ✅ | ✅ | ✅ | **❌** |

**Honest read:** ahead of Just/Task/mise on caching, remote, and plugins; on par
with moon/Turborepo on cache fundamentals; **behind** on toolchain (mise/moon),
editor intelligence (Turborepo), ecosystem interop (moon), and — most of all —
**adoption maturity** (everyone). The code is competitive; the *credibility* isn't
built yet.

## 3. The gaps that actually matter (ranked)

1. **Toolchain management** — the one true *feature* gap for a polyglot runner.
   Kills "works on my machine" across languages. mise/moon have it; we don't.
2. **Proof we're fast** — we claim "fast" with zero published benchmarks. A
   reproducible benchmark vs make/just/task/moon is cheap and disproportionately
   credible.
3. **Adoption basics** — not on crates.io yet, no docs site, ~one example, no LSP.
   The engine is invisible without these.
4. **Cache correctness depth** — IO-tracing / graduated hermeticity. A fast cache
   that's occasionally wrong is worse than no cache; this makes it unimpeachable.
5. **Robustness & frontier** — level-by-level scheduler (not a work-stealing
   ready-queue), no OpenTelemetry, REAPI interop for the bazel-remote ecosystem.

## 4. The campaign (prioritized)

### Tier 1 — Close the gap, prove the claims (do first)
- **Toolchain pinning** (`[toolchain]`): pin + auto-download language runtimes,
  inject into task PATH. ✅ **Shipped** (generic `.tar.gz` installer, templated
  URLs, verified end-to-end). Remaining: zip archives + built-in tool presets.
- **Benchmarks**: a reproducible suite vs make/just/task (+ moon where comparable),
  committed and documented. Turns "fast" from a claim into a number.
- **Publish to crates.io** (yatr + yatr-plugin) — already publish-ready; unblocks
  `cargo install yatr` and real adoption.

### Tier 2 — Unimpeachable correctness + delightful DX
- **IO-tracing / graduated hermeticity**: warn on undeclared reads/writes; opt-in
  sandbox mode. Cache correctness before scaling.
- **LSP for `yatr.toml`**: go-to-task, hover docs, diagnostics, dependency lenses.
- **Docs site + cookbook**: real-world recipes (Rust+JS+Docker, CI patterns,
  plugins), beyond the single Boutique Bouquet showcase.

### Tier 3 — Ecosystem & frontier
- **REAPI interop** (SHA-256 + protobuf ActionResult) — plug into bazel-remote /
  BuildBuddy. Ecosystem reach.
- **Work-stealing scheduler**: run any task whose deps are done, instead of strict
  level barriers — better wall-clock under uneven task durations.
- **OpenTelemetry spans** (one per task under a root invocation span).
- **Plugin depth**: cache-key contributions; a plugin registry / index.

## 5. How we'll know it worked (success metrics)
- `cargo install yatr` works; >0 external installs/stars trend up.
- Published benchmark table showing yatr competitive-or-faster on a real repo.
- A polyglot project (e.g. Boutique Bouquet) pins its toolchains in `yatr.toml` and
  a fresh checkout runs green with **no manual runtime installs**.
- Cache correctness: IO-tracing finds zero undeclared-output surprises in the
  dogfood configs.
- An editor (VS Code) gives autocomplete + go-to-task from the LSP.

## Caveats
- Competitor capabilities are a mid-2026 snapshot and move fast.
- "Best on technical merit" is an architecture-and-capability claim, not (yet) an
  adoption claim — Tier 1's publish + benchmarks are what start closing that.
