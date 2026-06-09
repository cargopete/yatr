# yatr 🦬

Yet another task runner. But this one's actually good.

```
yatr test          # Run tests
yatr build         # Build with dependencies
yatr --watch test  # Re-run on file changes
```

A fast, single-binary, polyglot task runner with a cache that actually tells the
truth: content-addressed, output-restoring, and shareable across machines and CI.

## Why yatr?

| Feature | Make | Just | cargo-make | **yatr** |
|---------|------|------|------------|------------|
| Simple config | ❌ | ✅ | ⚠️ | ✅ |
| Parallel execution | ❌ | ✅ | ✅ | ✅ |
| Content-addressed caching | ❌ | ❌ | ⚠️ | ✅ |
| Captures & restores outputs | ❌ | ❌ | ❌ | ✅ |
| Remote / shared cache | ❌ | ❌ | ❌ | ✅ |
| Signed cache (anti-poisoning) | ❌ | ❌ | ❌ | ✅ |
| Watch mode | ❌ | ❌ | ❌ | ✅ |
| Config schema + JSON output | ❌ | ❌ | ❌ | ✅ |
| Scripting | Shell | Shell | Multiple | Rhai |
| Cross-platform | ❌ | ⚠️ | ✅ | ✅ |
| Zero runtime deps | N/A | ✅ | ❌ | ✅ |

yatr sits in the sweet spot: **simpler than cargo-make**, **more powerful than just**.

## Installation

Build and install from source (requires a Rust toolchain):

```bash
cargo install --git https://github.com/cargopete/yatr
```

Or from a local clone:

```bash
git clone https://github.com/cargopete/yatr
cd yatr
cargo install --path .
```

## Quick Start

Create a `yatr.toml` in your project root:

```toml
[tasks.test]
desc = "Run tests"
run = ["cargo test"]

[tasks.build]
desc = "Build release"
depends = ["test"]
run = ["cargo build --release"]
```

Run tasks:

```bash
yatr test          # Run tests
yatr build         # Runs test first, then build
yatr run test build  # Run multiple tasks
yatr --dry-run build # Show execution plan
```

## Configuration

### Basic Tasks

```toml
[tasks.fmt]
desc = "Format code"
run = ["cargo fmt", "cargo clippy --fix --allow-dirty"]
```

### Task Dependencies

```toml
[tasks.check]
desc = "Full check pipeline"
depends = ["fmt", "lint", "test"]
run = []  # Just runs dependencies
```

### Parallel Execution

```toml
[tasks.lint]
desc = "Run all linters"
parallel = true
run = [
    "cargo fmt --check",
    "cargo clippy -- -D warnings",
    "cargo doc --no-deps"
]
```

### Environment Variables

```toml
[env]
RUST_LOG = "debug"
DATABASE_URL = "postgres://localhost/test"

[tasks.test]
env = { RUST_BACKTRACE = "1" }
run = ["cargo test"]
```

### Rhai Scripting

For complex logic, use inline Rhai scripts:

```toml
[tasks.bump-version]
desc = "Bump patch version"
script = '''
    let cargo = read_file("Cargo.toml");
    let toml = parse_toml(cargo);
    let version = toml["package"]["version"];
    let new_version = semver_bump(version, "patch");
    
    print(`Bumping ${version} -> ${new_version}`);
    
    let updated = cargo.replace(version, new_version);
    write_file("Cargo.toml", updated);
'''
```

Available Rhai functions:

| Function | Description |
|----------|-------------|
| `read_file(path)` | Read file contents |
| `write_file(path, content)` | Write file |
| `file_exists(path)` | Check if file exists |
| `exec(cmd)` | Run shell command |
| `glob(pattern)` | Find files matching pattern |
| `parse_json(str)` | Parse JSON string |
| `parse_toml(str)` | Parse TOML string |
| `semver_bump(ver, part)` | Bump version (major/minor/patch) |
| `get_env(key)` | Get environment variable |

### Caching

Tasks are cached by default based on:
- Task configuration (commands, env vars)
- Source file contents (if `sources` specified)

```toml
[tasks.build]
run = ["cargo build --release"]
sources = ["src/**/*.rs", "Cargo.toml", "Cargo.lock"]
outputs = ["target/release/myapp"]
```

Disable caching:

```toml
[tasks.deploy]
no_cache = true
run = ["./deploy.sh"]
```

### Watch Mode

Re-run tasks when files change:

```bash
yatr watch test
```

Configure watch patterns:

```toml
[tasks.test]
run = ["cargo test"]
watch = ["src/**/*.rs", "tests/**/*.rs"]
```

### Shell Mode

By default, commands run without a shell (cross-platform safe). Enable shell for pipes, redirects, etc:

```toml
[tasks.lines]
shell = true
run = ["find src -name '*.rs' | wc -l"]
```

Or per-invocation:

```bash
yatr --shell run lines
```

## CLI Reference

```
yatr [OPTIONS] [TASK]...
yatr <COMMAND>

Commands:
  run      Run one or more tasks
  list     List available tasks
  watch    Watch for changes and re-run
  graph    Show task dependency graph
  cache    Manage task cache
  init     Create yatr.toml template
  check    Validate configuration
  schema   Print the JSON Schema for yatr.toml
  affected List tasks affected by changes since a git ref

Options:
  -c, --config <PATH>  Config file path
  -v, --verbose        Verbose output
  -q, --quiet          Suppress output
      --cwd <DIR>      Working directory
      --no-color       Disable colors
  -h, --help           Print help
  -V, --version        Print version
```

### Examples

```bash
# Run tasks
yatr test                    # Run 'test' task
yatr run test build          # Run multiple tasks
yatr run --dry-run build     # Show plan without executing
yatr run --force build       # Ignore cache
yatr run --parallel 4 test   # Limit parallelism

# List tasks
yatr list                    # Show all tasks
yatr list --format json      # JSON output
yatr list --deps             # Show dependencies

# Watch mode
yatr watch test              # Re-run on changes
yatr watch --clear test      # Clear screen between runs

# Dependency graph
yatr graph                   # Show full graph
yatr graph build             # Graph for specific task
yatr graph --format dot build | dot -Tpng > graph.png

# Cache management
yatr cache stats             # Show cache statistics
yatr cache clear             # Clear all cached results
yatr cache clear build       # Clear cache for one task
yatr cache path              # Show cache directory

# Machine-readable output
yatr run --json test         # Structured JSON: per-task results + summary
yatr run --json --dry-run ci # JSON execution plan, without running

# Profiling
yatr run --profile trace.json ci   # Chrome trace of the run (chrome://tracing / Perfetto)

# Affected (monorepo)
yatr affected origin/main          # List tasks affected by changes since a git ref
yatr run --affected origin/main ci # Run only the affected subset of the requested tasks
```

## Affected detection (monorepo)

In a large repo you don't want to run everything on every change. `yatr affected
<git-ref>` lists the tasks touched by changes since a ref — a task is affected
when one of its `sources`/`watch` globs matches a changed file, and the result
propagates to everything that (transitively) depends on it.

```bash
yatr affected main                  # what would I need to run for this branch?
yatr affected HEAD~1 --format json  # machine-readable, for CI
yatr run --affected origin/main test lint build   # run only the affected ones
```

Caching already gives you *correctness* (unchanged tasks are cache hits); affected
detection adds *speed at scale* by not even considering tasks git says can't have
moved. A task that declares no `sources` is treated as always affected — declaring
`sources` is what unlocks skipping.

## Shared (remote) cache

Point yatr at a shared HTTP cache and a task built on one machine (or in CI) is
restored on the next, instead of rebuilt. It speaks a small REST protocol —
`GET`/`PUT`/`HEAD` on `<url>/ac/<key>` (action results) and `<url>/cas/<blob>`
(content blobs) — the same path layout as Bazel's HTTP cache, so it works
against an off-the-shelf blob store or a tiny server.

```toml
[settings.remote_cache]
url = "https://cache.example.com/yatr"
token_env = "YATR_CACHE_TOKEN"   # optional; bearer token read from this env var
sign_key_env = "YATR_CACHE_KEY"  # optional; shared secret for signing (see below)
read = true                      # pull from the remote on a local miss (default: true)
write = true                     # push after a successful run (default: true)
```

The cache is **content-addressed and machine-portable**: identical inputs produce
identical keys regardless of checkout path. A flaky or unreachable remote is
non-fatal — yatr logs a warning and runs the task locally. Keep secrets out of
the committed config by using the `*_env` options rather than inline values.

**Integrity & signing.** Cached output blobs are verified against their content
digests on download, so a tampered blob is rejected automatically. For defence
against a compromised cache poisoning *action results*, set `sign_key_env` to a
shared secret: yatr signs each action result with a keyed BLAKE3 MAC and rejects
any entry whose signature doesn't verify under your key.

## Editor integration

yatr ships a JSON Schema for `yatr.toml`, giving you autocomplete, hover docs,
and validation in any editor that supports schema-validated TOML.

```bash
yatr schema > yatr.schema.json   # regenerate the schema any time
```

Point your editor at it. With taplo / the **Even Better TOML** VS Code extension,
add a directive to the top of your `yatr.toml`:

```toml
#:schema ./yatr.schema.json
```

or associate it globally in VS Code settings:

```json
"evenBetterToml.schema.associations": { "yatr\\.toml$": "./yatr.schema.json" }
```

## Showcases

Real-world projects using yatr in production:

### Boutique Bouquet - Full-Stack E-Commerce Platform

**Stack**: Rust (Axum) backend + Next.js frontend + PostgreSQL + Docker

[Boutique Bouquet](https://github.com/cargopete/boutique-bouquet) is an e-commerce platform that uses yatr to orchestrate its entire development workflow across multiple languages and services.

**Key workflows enabled by yatr:**

- **Unified development experience**: Single command (`yatr dev-local`) starts PostgreSQL in Docker, Rust backend, and Next.js frontend simultaneously
- **Language-agnostic orchestration**: Manages Rust cargo commands, npm scripts, and Docker operations from one config
- **Comprehensive CI pipeline**: `yatr ci` runs formatting, linting, type-checking, tests, and builds across both backend and frontend
- **Flexible deployment targets**: Separate tasks for Docker-based and local development workflows

**Example task configuration:**

```toml
[tasks.dev-local]
desc = "Start DB in Docker, backend and frontend locally (parallel)"
run = ["sh", "-c", "docker-compose up -d postgres && (cd backend && cargo run) & (cd frontend && npm run dev) & wait"]

[tasks.ci]
desc = "Run full CI pipeline (checks + tests + build)"
depends = ["check", "test", "build"]

[tasks.test]
desc = "Run all tests (backend + frontend)"
depends = ["backend-test", "frontend-test"]
```

**Results**:
- Reduced onboarding time - new developers run `yatr dev-local` instead of memorizing 5+ commands
- Consistent CI/CD - same task definitions work locally and in CI
- 40+ tasks managing everything from database migrations to production builds

---

*Using yatr in your project? [Open a PR](https://github.com/cargopete/yatr) to add your showcase!*

## Full Configuration Reference

```toml
# Global environment variables
[env]
KEY = "value"

# Global settings
[settings]
cache = true              # Enable caching (default: true)
cache_dir = ".yatr"     # Cache directory
parallelism = 0           # Max parallel tasks (0 = CPU count)
watch_debounce_ms = 300   # Watch debounce delay

# Task definition
[tasks.example]
desc = "Task description"           # Optional description
run = ["cmd1", "cmd2"]              # Commands to run (or use 'script')
script = "..."                       # Rhai script (alternative to 'run')
depends = ["other-task"]             # Run these first
parallel = false                     # Run commands in parallel
env = { KEY = "value" }              # Task-specific env vars
cwd = "./subdir"                     # Working directory
shell = false                        # Use shell for commands
watch = ["**/*.rs"]                  # File patterns for watch mode
sources = ["src/**"]                 # Files affecting cache key
outputs = ["target/"]                # Output files/dirs
no_cache = false                     # Disable caching
allow_failure = false                # Continue on failure
timeout = 300                        # Timeout in seconds
```

## License

MIT OR Apache-2.0
