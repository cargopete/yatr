# YATR 🏞️

A modern, fast task runner for Rust projects.

```
yatr test        # Run tests
yatr build       # Build with dependencies
yatr --watch test  # Re-run on file changes
```

## Why YATR?

| Feature | Make | Just | cargo-make | **YATR** |
|---------|------|------|------------|------------|
| Simple config | ❌ | ✅ | ⚠️ | ✅ |
| Parallel execution | ❌ | ✅ | ✅ | ✅ |
| Task caching | ❌ | ❌ | ⚠️ | ✅ |
| Scripting | Shell | Shell | Multiple | Rhai |
| Cross-platform | ❌ | ⚠️ | ✅ | ✅ |
| Watch mode | ❌ | ❌ | ❌ | ✅ |
| Zero runtime deps | N/A | ✅ | ❌ | ✅ |

YATR sits in the sweet spot: **simpler than cargo-make**, **more powerful than just**.

## Installation

```bash
cargo install yatr
```

Or build from source:

```bash
git clone https://github.com/cargopete/steppe
cd steppe
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
  init     Create YATR.toml template
  check    Validate configuration

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
yatr cache path              # Show cache directory
```

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
