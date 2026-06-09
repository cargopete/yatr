//! WASM plugin runtime.
//!
//! A task can be implemented by a WebAssembly plugin (`wasm = "plugin.wasm"`)
//! instead of shell commands or a Rhai script. Plugins are run in a lightweight
//! pure-Rust interpreter ([`wasmi`]) and are **capability-sandboxed**: the only
//! thing imported into the module is yatr's own host ABI, so a plugin cannot
//! touch the filesystem, network, or clock unless we explicitly grant it.
//!
//! ## Plugin ABI
//!
//! A plugin is a wasm module that:
//! - exports its linear memory as `memory`,
//! - exports `run() -> i32` (the entry point; `0` = success, non-zero = failure),
//! - may import these host functions from module `"yatr"`:
//!   - `emit(ptr: i32, len: i32)` — append the UTF-8 string at `[ptr, ptr+len)`
//!     in the plugin's memory to the task's captured output.
//!   - `log(ptr: i32, len: i32)` — log the UTF-8 string as an info message.
//!   - `input_len() -> i32` — byte length of this task's input (JSON of
//!     `{ "task": <name>, "env": { … } }`).
//!   - `input_read(ptr: i32) -> i32` — copy the input bytes into the plugin's
//!     memory at `ptr`, returning the number of bytes written (`-1` on failure,
//!     e.g. if the memory isn't large enough).

use std::path::{Path, PathBuf};

use wasmi::{Caller, Engine, Extern, Linker, Module, Store};

use crate::error::{Result, YatrError};

/// Resolve a plugin reference to a local `.wasm` file path.
///
/// A reference may be:
/// - a local path (resolved relative to `cwd`),
/// - an `http(s)://` URL, or
/// - a `github:owner/repo@tag/asset.wasm` shorthand for a release asset.
///
/// Remote references are downloaded once into a local plugin cache and reused.
/// Remote plugins still run in the same capability sandbox, so an untrusted
/// plugin cannot escape.
pub async fn resolve_plugin(wasm_ref: &str, cwd: &Path, task_name: &str) -> Result<PathBuf> {
    let remote_url = if is_remote(wasm_ref) {
        Some(wasm_ref.to_string())
    } else if let Some(spec) = wasm_ref.strip_prefix("github:") {
        Some(github_release_url(spec).ok_or_else(|| YatrError::Plugin {
            task: task_name.to_string(),
            message: format!(
                "invalid github plugin locator '{wasm_ref}' \
                 (expected github:owner/repo@tag/asset.wasm)"
            ),
        })?)
    } else {
        None
    };

    if let Some(url) = remote_url {
        return fetch_cached(&url, &plugins_cache_dir(), task_name).await;
    }

    let p = Path::new(wasm_ref);
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    })
}

/// True if a plugin reference is fetched over the network rather than from disk.
#[must_use]
pub fn is_remote_ref(s: &str) -> bool {
    is_remote(s) || s.starts_with("github:")
}

fn is_remote(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Map `owner/repo@tag/asset.wasm` to its GitHub release-asset download URL.
fn github_release_url(spec: &str) -> Option<String> {
    let (repo, reference) = spec.split_once('@')?;
    let (owner, name) = repo.split_once('/')?;
    let (tag, asset) = reference.rsplit_once('/')?;
    if owner.is_empty() || name.is_empty() || tag.is_empty() || asset.is_empty() {
        return None;
    }
    Some(format!(
        "https://github.com/{owner}/{name}/releases/download/{tag}/{asset}"
    ))
}

/// Directory where downloaded plugins are cached. Overridable via
/// `YATR_PLUGIN_DIR` (useful for tests and reproducible CI).
fn plugins_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("YATR_PLUGIN_DIR") {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("", "", "yatr").map_or_else(
        || PathBuf::from(".yatr/plugins"),
        |d| d.cache_dir().join("plugins"),
    )
}

/// Download a plugin to `dir` (keyed by a hash of its URL) if not already
/// cached, returning the local path. URLs are assumed immutable — use a
/// versioned URL (e.g. a tagged release asset) to pick up a new plugin.
async fn fetch_cached(url: &str, dir: &Path, task_name: &str) -> Result<PathBuf> {
    let err = |message: String| YatrError::Plugin {
        task: task_name.to_string(),
        message,
    };

    std::fs::create_dir_all(dir).map_err(|e| err(format!("cannot create plugin cache: {e}")))?;
    let key = blake3::hash(url.as_bytes()).to_hex().to_string();
    let dest = dir.join(format!("{key}.wasm"));
    if dest.exists() {
        return Ok(dest);
    }

    let resp = reqwest::get(url)
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| err(format!("failed to download {url}: {e}")))?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| err(format!("failed to read {url}: {e}")))?;

    let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|e| err(format!("cannot write plugin: {e}")))?;
    std::fs::rename(&tmp, &dest).map_err(|e| err(format!("cannot store plugin: {e}")))?;
    Ok(dest)
}

/// Host-side state threaded through a plugin invocation.
#[derive(Default)]
struct PluginState {
    output: String,
    input: Vec<u8>,
}

/// Load and run a WASM plugin, returning whatever it emitted as output.
///
/// `input` is made available to the plugin via the `input_len`/`input_read`
/// host functions (yatr passes the task name and environment as JSON).
pub fn run_plugin(wasm_path: &Path, task_name: &str, input: &[u8]) -> Result<String> {
    let err = |message: String| YatrError::Plugin {
        task: task_name.to_string(),
        message,
    };

    let bytes = std::fs::read(wasm_path)
        .map_err(|e| err(format!("cannot read plugin {}: {e}", wasm_path.display())))?;

    let engine = Engine::default();
    let module =
        Module::new(&engine, &bytes[..]).map_err(|e| err(format!("invalid module: {e}")))?;

    let mut store = Store::new(
        &engine,
        PluginState {
            output: String::new(),
            input: input.to_vec(),
        },
    );
    let mut linker = <Linker<PluginState>>::new(&engine);

    linker
        .func_wrap(
            "yatr",
            "emit",
            |mut caller: Caller<'_, PluginState>, ptr: i32, len: i32| {
                if let Some(s) = read_string(&caller, ptr, len) {
                    caller.data_mut().output.push_str(&s);
                }
            },
        )
        .map_err(|e| err(format!("failed to define host fn: {e}")))?;

    linker
        .func_wrap(
            "yatr",
            "log",
            |caller: Caller<'_, PluginState>, ptr: i32, len: i32| {
                if let Some(s) = read_string(&caller, ptr, len) {
                    tracing::info!("[plugin] {s}");
                }
            },
        )
        .map_err(|e| err(format!("failed to define host fn: {e}")))?;

    linker
        .func_wrap(
            "yatr",
            "input_len",
            |caller: Caller<'_, PluginState>| -> i32 {
                i32::try_from(caller.data().input.len()).unwrap_or(i32::MAX)
            },
        )
        .map_err(|e| err(format!("failed to define host fn: {e}")))?;

    linker
        .func_wrap(
            "yatr",
            "input_read",
            |mut caller: Caller<'_, PluginState>, ptr: i32| -> i32 {
                let data = caller.data().input.clone();
                let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
                    return -1;
                };
                let Ok(offset) = usize::try_from(ptr) else {
                    return -1;
                };
                match memory.write(&mut caller, offset, &data) {
                    Ok(()) => i32::try_from(data.len()).unwrap_or(i32::MAX),
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| err(format!("failed to define host fn: {e}")))?;

    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .map_err(|e| err(format!("instantiation failed: {e}")))?;

    let run = instance
        .get_typed_func::<(), i32>(&store, "run")
        .map_err(|e| err(format!("plugin must export `run() -> i32`: {e}")))?;

    let code = run
        .call(&mut store, ())
        .map_err(|e| err(format!("plugin trapped: {e}")))?;

    if code != 0 {
        return Err(err(format!("plugin returned non-zero status {code}")));
    }

    Ok(std::mem::take(&mut store.data_mut().output))
}

/// Read a UTF-8 string out of the plugin's exported `memory`.
fn read_string(caller: &Caller<'_, PluginState>, ptr: i32, len: i32) -> Option<String> {
    let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
        return None;
    };
    let start = usize::try_from(ptr).ok()?;
    let end = start.checked_add(usize::try_from(len).ok()?)?;
    let bytes = memory.data(caller).get(start..end)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(wat: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.wasm");
        std::fs::write(&path, wat::parse_str(wat).unwrap()).unwrap();
        (dir, path)
    }

    #[test]
    fn plugin_emits_output() {
        let (_dir, path) = plugin(
            r#"(module
                (import "yatr" "emit" (func $emit (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "hello from wasm")
                (func (export "run") (result i32)
                    (call $emit (i32.const 0) (i32.const 15))
                    (i32.const 0)))"#,
        );
        assert_eq!(run_plugin(&path, "t", b"").unwrap(), "hello from wasm");
    }

    #[test]
    fn plugin_nonzero_status_is_error() {
        let (_dir, path) = plugin(
            r#"(module
                (memory (export "memory") 1)
                (func (export "run") (result i32) (i32.const 1)))"#,
        );
        assert!(run_plugin(&path, "t", b"").is_err());
    }

    #[test]
    fn plugin_reads_its_input() {
        // Reads the input into memory at offset 256 and emits it back.
        let (_dir, path) = plugin(
            r#"(module
                (import "yatr" "emit" (func $emit (param i32 i32)))
                (import "yatr" "input_len" (func $input_len (result i32)))
                (import "yatr" "input_read" (func $input_read (param i32) (result i32)))
                (memory (export "memory") 1)
                (func (export "run") (result i32)
                    (local $len i32)
                    (local.set $len (call $input_len))
                    (drop (call $input_read (i32.const 256)))
                    (call $emit (i32.const 256) (local.get $len))
                    (i32.const 0)))"#,
        );
        let out = run_plugin(&path, "t", br#"{"task":"t","env":{"K":"V"}}"#).unwrap();
        assert_eq!(out, r#"{"task":"t","env":{"K":"V"}}"#);
    }

    #[test]
    fn github_locator_parsing() {
        assert_eq!(
            github_release_url("octocat/tools@v1.2.0/codegen.wasm").unwrap(),
            "https://github.com/octocat/tools/releases/download/v1.2.0/codegen.wasm"
        );
        // Tags may contain slashes; the asset is the last path segment.
        assert_eq!(
            github_release_url("o/r@release/v1/p.wasm").unwrap(),
            "https://github.com/o/r/releases/download/release/v1/p.wasm"
        );
        // Malformed locators are rejected.
        assert!(github_release_url("noatsign.wasm").is_none());
        assert!(github_release_url("owner@tag/asset.wasm").is_none()); // no repo
        assert!(github_release_url("owner/repo@tagonly").is_none()); // no asset
        assert!(is_remote_ref("github:o/r@v1/p.wasm"));
        assert!(!is_remote_ref("./local.wasm"));
    }

    #[tokio::test]
    async fn fetches_and_runs_remote_plugin() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let wasm = wat::parse_str(
            r#"(module
                (import "yatr" "emit" (func $emit (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "remote!")
                (func (export "run") (result i32)
                    (call $emit (i32.const 0) (i32.const 7))
                    (i32.const 0)))"#,
        )
        .unwrap();

        let server = MockServer::start().await;
        // Expect exactly ONE download even across two resolves (caching works).
        Mock::given(method("GET"))
            .and(path("/p.wasm"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(wasm))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let url = format!("{}/p.wasm", server.uri());

        let p1 = fetch_cached(&url, dir.path(), "t").await.unwrap();
        let p2 = fetch_cached(&url, dir.path(), "t").await.unwrap(); // served from cache
        assert_eq!(p1, p2);
        assert_eq!(run_plugin(&p1, "t", b"").unwrap(), "remote!");
    }

    #[test]
    fn plugin_cannot_touch_the_host() {
        // A module importing anything outside the `yatr` ABI (here, a WASI fn)
        // must fail to instantiate — the sandbox grants nothing else.
        let (_dir, path) = plugin(
            r#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func (param i32)))
                (memory (export "memory") 1)
                (func (export "run") (result i32) (i32.const 0)))"#,
        );
        assert!(run_plugin(&path, "t", b"").is_err());
    }
}
