# Caching

yatr's cache is **content-addressed**: a task's cache key is the BLAKE3 hash of
its commands, environment, working directory, and the **contents** of its
declared `sources`. Unchanged inputs → a cache hit; changed inputs → a real run.

The environment is the one the task runs with: the global `[env]` merged with
the task's own, plus the versions and URLs of any pinned `[toolchain]`s. The
ambient environment is not an input unless you name it:

```toml
[tasks.wasm]
sources = ["src/**"]
env_inputs = ["CLANG", "PATH"]   # a different compiler on PATH busts the key
run = ["./build.sh"]
```

A tool's version is not an input by default: a task keys on the command string,
not on which binary it resolves to. Name a command in `tool_inputs` and its
stdout joins the key, so upgrading the tool re-runs the task:

```toml
[tasks.drift]
sources = ["nuthatch.toml", "schema/**"]
tool_inputs = ["nuthatch --version"]
run = ["nuthatch schema", "git diff --exit-code"]
```

Each command runs directly, not through a shell, in the task's directory and
environment (so a pinned `[toolchain]` is on `PATH`). It runs once per yatr
invocation however many tasks name it, and a command that fails stops the run.
Only the command string and its stdout are hashed, never the binary's path, so
keys stay portable to a remote cache.

A task's key also folds in the keys of its `depends`, so a dependent re-runs
whenever a dependency's inputs change. A dependency that is not cached
contributes the contents of its declared `outputs` as well.

**Only tasks with `sources` are cached.** A task without them has no declared
inputs, so nothing could ever invalidate its entry; it runs every time, and
`yatr check` warns if it declares `outputs`. Local entries are scoped to the
project directory, so two checkouts never serve each other's results; a
[remote cache](./remote-cache.md) shares keys across checkouts by design.

```toml
[tasks.build]
sources = ["src/**", "Cargo.toml", "Cargo.lock"]
outputs = ["target/release/app"]
run = ["cargo build --release"]
```

`sources`, `outputs` and `watch` share one glob dialect, matched against paths
relative to the task's directory: `*` and `?` stop at `/`, `**` crosses it, and
a leading `./` is ignored. So `*.js` means top-level files only, and `dist/**`
means everything under `dist`.

Sources respect `.gitignore`, in a git checkout or not, so build artifacts stay
out of wildcard matches. A pattern whose literal path is itself ignored, such as
`gen/schema.json` or `web/dist/**`, names generated input on purpose and is
hashed regardless. Outputs ignore `.gitignore` entirely.

Only each pattern's literal base is walked: `src/**` reads `src/` and nothing
beside it, however large. A pattern that starts with a wildcard, such as
`**/*.rs`, walks the whole task directory.

## Outputs are captured and restored

On success, the files matched by `outputs` are stored in a content-addressed
store. On a **cache hit, they're restored** — delete `target/`, get a hit, and
your artifacts come back. (Many runners "cache" only stdout and leave you with
nothing on disk; yatr restores the real outputs.)

An `outputs` pattern that captures no files after a successful run is reported
with a warning, since a hit would then restore nothing for it; the result is
still cached. `yatr check` cannot tell, as outputs exist only after a run.

Restored files keep their permission bits, so a cached binary is still
executable, and symlinks come back as symlinks (on Unix). Each file is written
beside its destination and renamed into place, so a hit over existing outputs
replaces them rather than writing through a symlink or into a running binary.

## Cache correctness

A fast cache that's occasionally wrong is worse than no cache. yatr keys on file
*contents* (not mtimes, which `git checkout` and clock skew break), and can warn
when a task writes outside what it declared:

```bash
yatr run --trace-io build   # "task 'build' wrote files not declared as `outputs`: …"
```

## Managing the cache

```bash
yatr cache stats      # entries + size
yatr cache clear      # clear this project's entries
yatr cache clear build  # clear one task in this project
yatr cache clear --all  # clear every project's entries
yatr cache path       # show the cache directory
```

Caching is on by default; disable per task with `no_cache = true` or globally with
`[settings] cache = false`. To share hits across machines, see the
[remote cache](./remote-cache.md).
