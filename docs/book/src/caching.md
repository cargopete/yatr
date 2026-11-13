# Caching

yatr's cache is **content-addressed**: a task's cache key is the BLAKE3 hash of
its commands, environment, working directory, and the **contents** of its
declared `sources`. Unchanged inputs → a cache hit; changed inputs → a real run.

```toml
[tasks.build]
sources = ["src/**", "Cargo.toml", "Cargo.lock"]
outputs = ["target/release/app"]
run = ["cargo build --release"]
```

## Outputs are captured and restored

On success, the files matched by `outputs` are stored in a content-addressed
store. On a **cache hit, they're restored** — delete `target/`, get a hit, and
your artifacts come back. (Many runners "cache" only stdout and leave you with
nothing on disk; yatr restores the real outputs.)

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
yatr cache clear      # clear everything
yatr cache clear build  # clear one task
yatr cache path       # show the cache directory
```

Caching is on by default; disable per task with `no_cache = true` or globally with
`[settings] cache = false`. To share hits across machines, see the
[remote cache](./remote-cache.md).
