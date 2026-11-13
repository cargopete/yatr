# Toolchains

Pin a language runtime and yatr downloads it once and puts it on the task `PATH`
— a fresh checkout runs green with no manual installs. Its bin directory goes in
front of the `PATH` the task would otherwise see, including one set in `[env]`.

```toml
[toolchain.node]
version = "20.11.0"
url = "https://nodejs.org/dist/v{version}/node-v{version}-{os}-{arch}.tar.gz"
bin = "node-v{version}-{os}-{arch}/bin"

[tasks.build]
run = ["node build.js"]   # uses the pinned node, wherever yatr runs
```

`{version}`, `{os}` (`linux`/`darwin`/`win`) and `{arch}` (`x64`/`arm64`) are
substituted into the `url` and `bin` templates — matching the common Node-style
release-asset naming.

Toolchains are cached under a local toolchains directory (override with
`YATR_TOOLCHAIN_DIR`) and installed once, then reused. `.tar.gz`/`.tgz` archives
are supported today.

This kills "works on my machine" across languages: the runtime your build needs
is declared in `yatr.toml` and fetched on demand, the same on every developer
machine and in CI.
