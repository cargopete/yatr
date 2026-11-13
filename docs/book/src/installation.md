# Installation

```bash
cargo install yatr
```

Or from source (latest `main`):

```bash
cargo install --git https://github.com/cargopete/yatr
```

## Quick start

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
yatr test            # run 'test'
yatr build           # runs 'test' first, then 'build'
yatr run test build  # run multiple
yatr --dry-run build # show the plan without executing
yatr list            # list available tasks
```

Bare task names are shorthand for `yatr run <task>`, and take the same options
either side of the names: `yatr build --dry-run` is `yatr run --dry-run build`.
