# Monorepos

## Affected detection

In a large repo you don't want to run everything on every change. `yatr affected
<git-ref>` lists the tasks touched by changes since a ref. Changes are taken from
the ref's merge-base with `HEAD` to the working tree, untracked files included, so
commits that landed on the ref after you branched don't count. A task is affected
when one of its `sources`/`watch` globs, relative to its `cwd`, matches a changed
file, or when its own definition in `yatr.toml` changed (`desc` aside; an edit to
global `env` counts for every task it reaches, a `[toolchain]` edit for all). The
result propagates to everything that (transitively) depends on it.

```bash
yatr affected main                     # what would I need to run for this branch?
yatr affected HEAD~1 --format json     # machine-readable, for CI
yatr run --affected origin/main test lint build   # run only the affected ones
```

Caching gives you *correctness* (unchanged tasks are cache hits); affected
detection adds *speed at scale* by not even considering tasks git says can't have
moved. A task that runs something but declares no `sources` is treated as always
affected — declaring `sources` is what unlocks skipping. An aggregate (`run = []`
with `depends`) is affected exactly when one of its dependencies is.

`run --affected` applies the filter to dependencies as well: an unaffected
dependency is skipped, not run, so on a fresh checkout an affected task cannot
rely on outputs from one that wasn't.

## Splitting config across files

Keep task definitions next to the code they build and compose them from a root
`yatr.toml` with `include`:

```toml
# yatr.toml
include = ["frontend/yatr.toml", "backend/yatr.toml"]

[tasks.build-all]
depends = ["fe-build", "be-build"]   # tasks defined in the included files
```

Includes are resolved relative to the including file and merged recursively
(cycles are detected). Tasks and `env` are composed; the root file's `settings`
are authoritative. A task's paths (`cwd`, `sources`, `outputs`, `watch`) are
relative to the file that defines it, so `frontend/yatr.toml` can say
`sources = ["src/**"]` and mean `frontend/src`. A task defined in two files is an error — names are global.
