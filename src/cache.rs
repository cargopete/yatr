//! Content-addressable cache for task results
//!
//! The cache is split into two stores, mirroring the Bazel Remote Execution
//! API so a remote backend can later be slotted in behind the same shapes:
//!
//! - **Action cache** (`ac/<key>.json`): an [`ActionResult`] per cache key,
//!   recording the task's captured stdout, exit success, duration, and the
//!   content digests of its declared output files.
//! - **Content-addressable store** (`cas/<blake3>`): the output file blobs,
//!   keyed by the BLAKE3 hash of their contents and shared across entries.
//!
//! A cache key is derived from the task name, its commands/script, environment,
//! working directory, shell mode, declared `outputs`, and the **contents** of
//! its `sources`. On a hit, the recorded outputs are restored to disk; if any
//! blob is missing the entry is treated as a miss and the task runs for real.

#![allow(clippy::missing_errors_doc)]

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    time::Duration,
};

use blake3::Hasher;
use globset::GlobSetBuilder;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{CacheProtocol, Config, TaskConfig};
use crate::error::{Result, YatrError};
use crate::globs;
use crate::reapi;
use crate::remote::RemoteCache;

/// A single cached output file: its path relative to the task's working
/// directory, and the BLAKE3 digest of its contents in the CAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEntry {
    pub path: String,
    pub blob: String,
    /// Unix permission bits, restored on a hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// The output is a symlink and the blob holds its target, not contents.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub symlink: bool,
}

/// Names the directory an action-cache scope belongs to.
const SCOPE_MARKER: &str = ".dir";

/// The result of executing a task, stored in the action cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    /// Hash of the cache key
    pub key: String,
    /// Task name
    pub task: String,
    /// Timestamp of creation
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Duration of the original execution, in milliseconds
    pub duration_ms: u64,
    /// Whether the original execution succeeded
    pub success: bool,
    /// Captured stdout of the task
    pub stdout: String,
    /// Declared output files captured into the CAS
    pub outputs: Vec<OutputEntry>,
}

/// On-disk/on-wire wrapper around an [`ActionResult`], carrying an optional
/// keyed-BLAKE3 MAC over the canonical `result` bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignedAc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sig: Option<String>,
    result: ActionResult,
}

/// Files matched by a set of `sources` patterns, paired with a per-pattern
/// flag recording whether that pattern matched anything at all.
type SourceMatches = (Vec<(String, PathBuf)>, Vec<bool>);

/// Task result cache
#[derive(Debug, Clone)]
pub struct Cache {
    /// Cache directory
    dir: PathBuf,
    /// Whether caching is enabled
    enabled: bool,
    /// Optional shared/remote backend
    remote: Option<RemoteCache>,
    /// Optional 32-byte key for signing/verifying action results
    signing_key: Option<[u8; 32]>,
}

impl Cache {
    /// Create a new cache instance
    pub fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = dir.unwrap_or_else(|| {
            directories::ProjectDirs::from("", "", "yatr").map_or_else(
                || PathBuf::from(".yatr/cache"),
                |d| d.cache_dir().to_path_buf(),
            )
        });

        std::fs::create_dir_all(dir.join("ac"))?;
        std::fs::create_dir_all(dir.join("cas"))?;

        Ok(Self {
            dir,
            enabled: true,
            remote: None,
            signing_key: None,
        })
    }

    /// Attach an optional remote backend (builder style).
    #[must_use]
    pub fn with_remote(mut self, remote: Option<RemoteCache>) -> Self {
        self.remote = remote;
        self
    }

    /// Attach an optional signing key, derived from a user secret (builder style).
    #[must_use]
    pub const fn with_signing_key(mut self, key: Option<[u8; 32]>) -> Self {
        self.signing_key = key;
        self
    }

    /// Derive a 32-byte signing key from a user-supplied secret string.
    #[must_use]
    pub fn derive_key(secret: &str) -> [u8; 32] {
        blake3::derive_key("yatr cache action-result signing v1", secret.as_bytes())
    }

    /// Create a disabled cache (no-op)
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            dir: PathBuf::new(),
            enabled: false,
            remote: None,
            signing_key: None,
        }
    }

    /// Check if cache is enabled
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a task's results are cached at all. A task without `sources`
    /// has no declared inputs, so nothing could ever invalidate its entry.
    #[must_use]
    pub const fn caches(task: &TaskConfig) -> bool {
        !task.no_cache && !task.sources.is_empty()
    }

    /// Look up a cached result under `key` (from [`Cache::compute_key`]) and,
    /// on a hit, restore its outputs.
    ///
    /// Returns `None` (a miss) when there is no entry, the entry is for a
    /// different task, or any recorded output blob is missing — in which case
    /// the caller should run the task for real.
    pub async fn get(&self, task_name: &str, key: &str, cwd: &Path) -> Result<Option<String>> {
        if !self.enabled {
            return Ok(None);
        }

        // Local action cache first.
        let result = if let Some(result) = self.load_local_ac(key, task_name, cwd) {
            Some(result)
        } else {
            // Local miss: try the remote, populating the local cache on a hit.
            self.fetch_from_remote(key, task_name, cwd).await
        };

        let Some(result) = result else {
            return Ok(None);
        };

        // Restore declared outputs. If any blob is missing, the cache is
        // incomplete: fall through to a real run rather than lie.
        if !self.restore_outputs(cwd, &result.outputs)? {
            return Ok(None);
        }

        Ok(Some(result.stdout))
    }

    /// Load and validate a local action-cache entry for `key`.
    fn load_local_ac(&self, key: &str, task_name: &str, cwd: &Path) -> Option<ActionResult> {
        let bytes = std::fs::read(self.ac_path(key, cwd)).ok()?;
        self.extract_verified(&bytes, task_name)
    }

    /// Sign action-result bytes with the keyed MAC, if a signing key is set.
    fn sign(&self, result_bytes: &[u8]) -> Option<String> {
        self.signing_key
            .map(|k| blake3::keyed_hash(&k, result_bytes).to_hex().to_string())
    }

    /// Parse a stored/received [`SignedAc`], verify its MAC (when a signing key
    /// is configured), and return the inner result if it is valid and matches
    /// the expected task. A signature mismatch is rejected loudly.
    fn extract_verified(&self, bytes: &[u8], task_name: &str) -> Option<ActionResult> {
        let signed = serde_json::from_slice::<SignedAc>(bytes).ok()?;

        if let Some(key) = self.signing_key {
            let result_bytes = serde_json::to_vec(&signed.result).ok()?;
            let expected = blake3::keyed_hash(&key, &result_bytes);
            // Constant-time comparison via blake3::Hash equality.
            let ok = signed
                .sig
                .as_deref()
                .and_then(|s| blake3::Hash::from_hex(s).ok())
                .is_some_and(|got| got == expected);
            if !ok {
                tracing::warn!(
                    "cache signature verification failed for task '{task_name}' — rejecting entry"
                );
                return None;
            }
        }

        (signed.result.task == task_name && signed.result.success).then_some(signed.result)
    }

    /// On a local miss, try the remote: download the action result and any
    /// referenced blobs into the local store. Remote failures are non-fatal —
    /// they degrade to a miss, never an error.
    async fn fetch_from_remote(
        &self,
        key: &str,
        task_name: &str,
        cwd: &Path,
    ) -> Option<ActionResult> {
        let remote = self.remote.as_ref()?;
        if !remote.read {
            return None;
        }
        if remote.protocol == CacheProtocol::Reapi {
            return Self::fetch_reapi(remote, key, cwd).await;
        }

        let ac_bytes = match remote.get_ac(key).await {
            Ok(bytes) => bytes?,
            Err(e) => {
                tracing::warn!("remote cache read failed for {key}: {e}");
                return None;
            }
        };

        // Verify the action result's signature before trusting any of it.
        let result = self.extract_verified(&ac_bytes, task_name)?;

        // Ensure every referenced blob is present locally.
        for entry in &result.outputs {
            let local = self.cas_path(&entry.blob);
            if local.exists() {
                continue;
            }
            match remote.get_cas(&entry.blob).await {
                Ok(Some(bytes)) => {
                    // Blobs are content-addressed: a digest mismatch means the
                    // remote served tampered or corrupt data — reject it.
                    if blake3::hash(&bytes).to_hex().to_string() != entry.blob {
                        tracing::warn!(
                            "remote blob {} failed integrity check — rejecting entry",
                            entry.blob
                        );
                        return None;
                    }
                    if Self::write_atomic(&local, &bytes).is_err() {
                        return None;
                    }
                }
                // Missing blob or transport error → unusable entry.
                Ok(None) | Err(_) => return None,
            }
        }

        // Persist the action result locally for next time.
        self.write_ac(key, cwd, &ac_bytes).ok()?;
        Some(result)
    }

    /// Store a successful task result, capturing its declared outputs.
    pub async fn put(
        &self,
        task_name: &str,
        key: &str,
        config: &TaskConfig,
        cwd: &Path,
        stdout: &str,
        duration: Duration,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let key = key.to_string();
        let outputs = self.capture_outputs(cwd, &config.outputs)?;

        let result = ActionResult {
            key: key.clone(),
            task: task_name.to_string(),
            created_at: chrono::Utc::now(),
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            success: true,
            stdout: stdout.to_string(),
            outputs,
        };

        // Sign the canonical result, then wrap and store.
        let result_bytes = serde_json::to_vec(&result).map_err(|e| Self::ser_err(&e))?;
        let signed = SignedAc {
            sig: self.sign(&result_bytes),
            result,
        };
        let bytes = serde_json::to_vec_pretty(&signed).map_err(|e| Self::ser_err(&e))?;

        self.write_ac(&key, cwd, &bytes)?;

        // Write-through to the remote (non-fatal).
        self.upload_to_remote(&key, &signed.result, bytes, cwd)
            .await;
        Ok(())
    }

    fn ser_err(e: &serde_json::Error) -> YatrError {
        YatrError::Cache {
            message: format!("Failed to serialize action result: {e}"),
        }
    }

    /// Upload an action result and its blobs to the remote cache. Best-effort:
    /// any failure is logged and swallowed so it can't break the build.
    async fn upload_to_remote(
        &self,
        key: &str,
        result: &ActionResult,
        ac_bytes: Vec<u8>,
        cwd: &Path,
    ) {
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        if !remote.write {
            return;
        }
        if remote.protocol == CacheProtocol::Reapi {
            self.upload_reapi(remote, key, result, cwd).await;
            return;
        }

        for entry in &result.outputs {
            // Skip blobs the remote already has.
            if remote.has_cas(&entry.blob).await.unwrap_or(false) {
                continue;
            }
            let Ok(bytes) = std::fs::read(self.cas_path(&entry.blob)) else {
                continue;
            };
            if let Err(e) = remote.put_cas(&entry.blob, bytes).await {
                tracing::warn!("remote cache blob upload failed for {}: {e}", entry.blob);
                return; // don't publish an action result with missing blobs
            }
        }

        if let Err(e) = remote.put_ac(key, ac_bytes).await {
            tracing::warn!("remote cache action upload failed for {key}: {e}");
        }
    }

    /// Upload using the REAPI wire format: SHA-256 CAS blobs + a protobuf
    /// `ActionResult` under a SHA-256 action key. Interoperates with
    /// `bazel-remote` / `BuildBuddy` as yatr's shared cache backend.
    async fn upload_reapi(
        &self,
        remote: &RemoteCache,
        key: &str,
        result: &ActionResult,
        cwd: &Path,
    ) {
        let ac_key = reapi::sha256_hex(key.as_bytes());
        let mut files = Vec::new();
        let mut symlinks = Vec::new();
        for entry in &result.outputs {
            let Ok(bytes) = std::fs::read(self.cas_path(&entry.blob)) else {
                return;
            };
            if entry.symlink {
                symlinks.push(reapi::OutputSymlink {
                    path: entry.path.clone(),
                    target: String::from_utf8_lossy(&bytes).into_owned(),
                });
                continue;
            }
            let digest = reapi::sha256_hex(&bytes);
            let size = bytes.len() as u64;
            let executable = entry.mode.is_some_and(|m| m & 0o111 != 0);
            if !remote.has_cas(&digest).await.unwrap_or(false) {
                if let Err(e) = remote.put_cas(&digest, bytes).await {
                    tracing::warn!("reapi blob upload failed: {e}");
                    return;
                }
            }
            files.push(reapi::OutputFile {
                path: entry.path.clone(),
                digest,
                size,
                executable,
            });
        }
        let ar = reapi::ActionResult {
            output_files: files,
            output_symlinks: symlinks,
            exit_code: 0,
            stdout: result.stdout.clone().into_bytes(),
        };
        if let Err(e) = remote
            .put_ac(&ac_key, reapi::encode_action_result(&ar))
            .await
        {
            tracing::warn!("reapi action upload failed: {e}");
        }
    }

    /// Fetch + restore from a REAPI cache. Output files are written directly to
    /// `cwd` (verified against their SHA-256 digests); the returned result has no
    /// local outputs, so the caller's restore step is a no-op.
    async fn fetch_reapi(remote: &RemoteCache, key: &str, cwd: &Path) -> Option<ActionResult> {
        let ac_key = reapi::sha256_hex(key.as_bytes());
        let ac_bytes = match remote.get_ac(&ac_key).await {
            Ok(bytes) => bytes?,
            Err(e) => {
                tracing::warn!("reapi read failed for {ac_key}: {e}");
                return None;
            }
        };
        let ar = reapi::decode_action_result(&ac_bytes)?;

        for f in &ar.output_files {
            let Ok(Some(bytes)) = remote.get_cas(&f.digest).await else {
                return None;
            };
            if reapi::sha256_hex(&bytes) != f.digest {
                tracing::warn!("reapi blob {} failed integrity check", f.digest);
                return None;
            }
            let dest = cwd.join(&f.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).ok()?;
            }
            Self::write_atomic(&dest, &bytes).ok()?;
            if f.executable {
                Self::set_executable(&dest);
            }
        }
        for l in &ar.output_symlinks {
            let dest = cwd.join(&l.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).ok()?;
            }
            let tmp = Self::tmp_path(&dest);
            Self::make_symlink(&l.target, &tmp).ok()?;
            std::fs::rename(&tmp, &dest).ok()?;
        }

        Some(ActionResult {
            key: key.to_string(),
            task: String::new(),
            created_at: chrono::Utc::now(),
            duration_ms: 0,
            success: true,
            stdout: String::from_utf8_lossy(&ar.stdout).into_owned(),
            outputs: Vec::new(),
        })
    }

    #[cfg(unix)]
    fn set_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(path) {
            let mut perms = md.permissions();
            perms.set_mode(perms.mode() | 0o755);
            let _ = std::fs::set_permissions(path, perms);
        }
    }

    #[cfg(not(unix))]
    fn set_executable(_path: &Path) {}

    /// Invalidate the cached entry for a specific task + input combination.
    // Async by design: a remote (REAPI) backend will perform network I/O here.
    #[allow(clippy::unused_async)]
    pub async fn invalidate(&self, key: &str, cwd: &Path) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let ac_path = self.ac_path(key, cwd);
        if ac_path.exists() {
            std::fs::remove_file(&ac_path)?;
        }
        Ok(())
    }

    /// Clear the entire cache (both action cache and CAS).
    pub async fn clear(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        if self.dir.exists() {
            tokio::fs::remove_dir_all(&self.dir).await?;
        }
        std::fs::create_dir_all(self.dir.join("ac"))?;
        std::fs::create_dir_all(self.dir.join("cas"))?;
        Ok(())
    }

    /// Remove action-cache entries, optionally only those for tasks under
    /// `root` and only those for one named task, regardless of inputs.
    ///
    /// Returns the number of entries removed. Orphaned CAS blobs are left in
    /// place (cheap, content-addressed, and reused by other entries); a full
    /// `clear` reclaims them. Entries from before entries were scoped by
    /// directory belong to no project, so only an unscoped clear removes them.
    pub fn clear_entries(&self, root: Option<&Path>, task_name: Option<&str>) -> Result<usize> {
        if !self.enabled {
            return Ok(0);
        }
        let root = root.map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| r.to_path_buf()));

        let mut removed = 0;
        for (scope, path) in self.ac_entries()? {
            if let Some(root) = &root {
                if !scope.as_ref().is_some_and(|dir| dir.starts_with(root)) {
                    continue;
                }
            }
            if let Some(task_name) = task_name {
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let is_task = serde_json::from_str::<SignedAc>(&content)
                    .is_ok_and(|signed| signed.result.task == task_name);
                if !is_task {
                    continue;
                }
            }
            std::fs::remove_file(&path)?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Every action-cache entry, with the directory it is scoped to (`None`
    /// for an entry written before entries were scoped).
    fn ac_entries(&self) -> Result<Vec<(Option<PathBuf>, PathBuf)>> {
        let ac_dir = self.dir.join("ac");
        let mut entries = Vec::new();
        if !ac_dir.exists() {
            return Ok(entries);
        }
        let is_entry = |p: &Path| p.extension().is_some_and(|e| e == "json");
        for entry in std::fs::read_dir(&ac_dir)? {
            let path = entry?.path();
            if path.is_dir() {
                let scope = std::fs::read_to_string(path.join(SCOPE_MARKER))
                    .ok()
                    .map(|s| PathBuf::from(s.trim_end_matches('\n')));
                for inner in std::fs::read_dir(&path)? {
                    let inner = inner?.path();
                    if is_entry(&inner) {
                        entries.push((scope.clone(), inner));
                    }
                }
            } else if is_entry(&path) {
                entries.push((None, path));
            }
        }
        Ok(entries)
    }

    /// Get cache statistics
    pub fn stats(&self) -> Result<CacheStats> {
        if !self.enabled {
            return Ok(CacheStats::default());
        }

        let mut total_size = 0u64;
        let mut entry_count = 0usize;

        for (_, path) in self.ac_entries()? {
            total_size += std::fs::metadata(&path)?.len();
            entry_count += 1;
        }

        let cas_dir = self.dir.join("cas");
        if cas_dir.exists() {
            for entry in std::fs::read_dir(&cas_dir)? {
                total_size += entry?.metadata()?.len();
            }
        }

        Ok(CacheStats {
            entries: entry_count,
            total_size,
            cache_dir: self.dir.clone(),
        })
    }

    /// Bumped when the shape of a stored entry or of the key changes, so
    /// entries written by an older yatr are never served.
    const KEY_VERSION: &'static [u8] = b"yatr-cache-v3";

    /// Compute the cache key for a task of `project`, given the
    /// [`Cache::fingerprint`]s of its dependencies.
    ///
    /// The key is portable: it holds nothing machine-specific, so a remote
    /// cache can share it. Local entries are scoped to `cwd` on top of it.
    pub fn compute_key(
        task_name: &str,
        config: &TaskConfig,
        project: &Config,
        deps: &[String],
        cwd: &Path,
    ) -> Result<String> {
        let mut hasher = Hasher::new();

        hasher.update(Self::KEY_VERSION);
        hasher.update(task_name.as_bytes());

        for cmd in &config.run {
            hasher.update(cmd.as_bytes());
        }
        if let Some(script) = &config.script {
            hasher.update(script.as_bytes());
        }
        if let Some(wasm) = &config.wasm {
            hasher.update(wasm.to_string_lossy().as_bytes());
        }

        // The task's *declared* (relative) working directory and shell mode
        // change command semantics. We deliberately hash the relative `cwd`
        // rather than the absolute one so keys are portable across machines —
        // a prerequisite for a shared remote cache.
        if let Some(rel) = &config.cwd {
            hasher.update(rel.to_string_lossy().as_bytes());
        }
        hasher.update(&[u8::from(config.shell.unwrap_or(false))]);

        // The environment the config declares: global `[env]` merged with the
        // task's own. Toolchain bin dirs are machine paths, so the pins below
        // stand in for them.
        let env = project.declared_env(config);
        let mut env_pairs: Vec<_> = env.iter().collect();
        env_pairs.sort_by_key(|(k, _)| *k);
        for (k, v) in env_pairs {
            Self::update_field(&mut hasher, k.as_bytes());
            Self::update_field(&mut hasher, v.as_bytes());
        }

        let mut toolchains: Vec<_> = project.toolchain.iter().collect();
        toolchains.sort_by_key(|(k, _)| *k);
        for (name, tc) in toolchains {
            for part in [
                Some(name.as_str()),
                Some(tc.version.as_str()),
                Some(tc.url.as_str()),
                tc.bin.as_deref(),
                tc.sha256.as_deref(),
            ] {
                Self::update_field(&mut hasher, part.unwrap_or_default().as_bytes());
            }
        }

        // Declared ambient inputs; unset hashes differently from empty.
        let mut env_inputs = config.env_inputs.clone();
        env_inputs.sort();
        for name in &env_inputs {
            Self::update_field(&mut hasher, name.as_bytes());
            match std::env::var_os(name) {
                Some(v) => Self::update_field(&mut hasher, v.as_encoded_bytes()),
                None => {
                    hasher.update(&[0xff]);
                }
            }
        }

        let mut deps = deps.to_vec();
        deps.sort();
        for dep in &deps {
            Self::update_field(&mut hasher, dep.as_bytes());
        }

        // Declared output patterns (sorted) — changing them changes the action.
        let mut outputs = config.outputs.clone();
        outputs.sort();
        for pattern in &outputs {
            hasher.update(pattern.as_bytes());
        }

        // Contents of source files.
        if !config.sources.is_empty() {
            let source_hash = Self::hash_sources(cwd, &config.sources)?;
            hasher.update(source_hash.as_bytes());
        }

        Ok(hasher.finalize().to_hex()[..16].to_string())
    }

    fn update_field(hasher: &mut Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }

    /// What a dependent folds into its key for this task: its key, plus the
    /// contents of its outputs when it is not cached, since then the key
    /// alone does not capture what it produced.
    #[must_use]
    pub fn fingerprint(key: &str, config: &TaskConfig, cwd: &Path) -> String {
        if Self::caches(config) {
            return key.to_string();
        }
        let mut hasher = Hasher::new();
        hasher.update(key.as_bytes());
        for path in Self::collect_output_files(cwd, &config.outputs) {
            let rel = path.strip_prefix(cwd).unwrap_or(&path);
            Self::update_field(&mut hasher, rel.to_string_lossy().as_bytes());
            let content = std::fs::read_link(&path)
                .map(|t| t.to_string_lossy().into_owned().into_bytes())
                .or_else(|_| std::fs::read(&path))
                .unwrap_or_default();
            Self::update_field(&mut hasher, &content);
        }
        hasher.finalize().to_hex()[..16].to_string()
    }

    /// Directories never walked when collecting sources, no matter what the
    /// patterns say. Both hold state that changes underneath a task without
    /// the task's inputs having changed: `.git` moves on every commit, and
    /// `.yatr` is this cache's own on-disk state when `cache_dir` is local.
    /// Hashing either would make every key unstable.
    const NEVER_WALKED: &'static [&'static str] = &[".git", ".yatr"];

    /// Walk `cwd` and collect the files matching `patterns`, returning them
    /// alongside a per-pattern flag saying whether that pattern matched
    /// anything. Hidden entries are walked: a `sources` glob pointing at
    /// `.github/`, `.cargo/` or similar is a legitimate declared input, and
    /// skipping it silently collapsed the cache key (see `hash_sources`).
    ///
    /// `.gitignore` keeps build artifacts out of wildcard matches, but a
    /// pattern whose literal path is itself ignored (`gen/schema.json`,
    /// `web/dist/**`) names generated input on purpose, so it is walked
    /// without ignore rules.
    fn collect_sources(cwd: &Path, patterns: &[String]) -> Result<SourceMatches> {
        let mut builder = GlobSetBuilder::new();
        let mut per_pattern = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let glob = globs::compile(pattern).map_err(|e| YatrError::Cache {
                message: format!("Invalid glob pattern '{pattern}': {e}"),
            })?;
            per_pattern.push(glob.compile_matcher());
            builder.add(glob);
        }
        let globset = builder.build().map_err(|e| YatrError::Cache {
            message: format!("Failed to build glob set: {e}"),
        })?;
        let bases: Vec<PathBuf> = patterns
            .iter()
            .map(|p| cwd.join(globs::literal_base(p).0))
            .collect();
        let mut base_seen = vec![false; patterns.len()];

        let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut matched = vec![false; patterns.len()];
        let mut consider = |path: &Path, only: Option<usize>| {
            let Ok(rel) = path.strip_prefix(cwd) else {
                return;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let mut hit = false;
            for (i, m) in per_pattern.iter().enumerate() {
                if only.is_none_or(|o| o == i) && m.is_match(&rel_str) {
                    matched[i] = true;
                    hit = true;
                }
            }
            if hit {
                files.insert(rel_str, path.to_path_buf());
            }
        };

        for entry in Self::source_walk(cwd, &bases).filter_map(std::result::Result::ok) {
            let path = entry.path();
            for (seen, base) in base_seen.iter_mut().zip(&bases) {
                *seen |= path == base;
            }
            if path.is_file() && globset.is_match(path.strip_prefix(cwd).unwrap_or(path)) {
                consider(path, None);
            }
        }

        for (i, base) in bases.iter().enumerate() {
            if base_seen[i]
                || !base.exists()
                || Self::never_walked(base.strip_prefix(cwd).unwrap_or(base))
            {
                continue;
            }
            for entry in WalkDir::new(base)
                .into_iter()
                .filter_entry(|e| !Self::never_walked(Path::new(e.file_name())))
                .filter_map(std::result::Result::ok)
            {
                if entry.path().is_file() {
                    consider(entry.path(), Some(i));
                }
            }
        }

        // A BTreeMap keeps the files sorted by relative path, for a deterministic hash.
        Ok((files.into_iter().collect(), matched))
    }

    /// Walk `cwd` honouring ignore files, descending only into directories on
    /// the way to or below one of `bases`.
    fn source_walk(cwd: &Path, bases: &[PathBuf]) -> ignore::Walk {
        let bases = bases.to_vec();
        // `hidden(false)` walks dotfiles and dot-directories. `require_git(false)`
        // honours `.gitignore` outside a git checkout too.
        WalkBuilder::new(cwd)
            .hidden(false)
            .require_git(false)
            .filter_entry(move |e| {
                !Self::never_walked(Path::new(e.file_name()))
                    && bases
                        .iter()
                        .any(|b| b.starts_with(e.path()) || e.path().starts_with(b))
            })
            .build()
    }

    fn never_walked(rel: &Path) -> bool {
        rel.components().any(|c| {
            Self::NEVER_WALKED
                .iter()
                .any(|skip| c.as_os_str() == std::ffi::OsStr::new(skip))
        })
    }

    /// The `sources` patterns that match no file under `cwd`. A pattern that
    /// matches nothing contributes nothing to the cache key, which is nearly
    /// always a typo rather than an intention, so `yatr check` reports them.
    pub fn unmatched_source_patterns(cwd: &Path, patterns: &[String]) -> Result<Vec<String>> {
        let (_, matched) = Self::collect_sources(cwd, patterns)?;
        Ok(patterns
            .iter()
            .zip(matched)
            .filter(|(_, m)| !*m)
            .map(|(p, _)| p.clone())
            .collect())
    }

    /// Hash the contents of source files matching the glob patterns, rooted at
    /// `cwd` and respecting `.gitignore` (so build artifacts and `node_modules`
    /// don't bloat or destabilise the key).
    fn hash_sources(cwd: &Path, patterns: &[String]) -> Result<String> {
        let (files, _) = Self::collect_sources(cwd, patterns)?;

        let mut hasher = Hasher::new();
        for (rel, path) in files {
            hasher.update(rel.as_bytes());
            let content = std::fs::read(&path).unwrap_or_default();
            hasher.update(&content);
        }

        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Capture the files matched by the output patterns into the CAS.
    fn capture_outputs(&self, cwd: &Path, patterns: &[String]) -> Result<Vec<OutputEntry>> {
        let mut entries = Vec::new();
        for path in Self::collect_output_files(cwd, patterns) {
            let Ok(rel) = path.strip_prefix(cwd) else {
                continue;
            };
            let meta = std::fs::symlink_metadata(&path)?;
            let symlink = cfg!(unix) && meta.file_type().is_symlink();
            let (content, mode) = if symlink {
                let target = std::fs::read_link(&path)?;
                (target.to_string_lossy().into_owned().into_bytes(), None)
            } else {
                (std::fs::read(&path)?, Self::mode_of(&meta))
            };
            let blob = self.store_blob(&content)?;
            entries.push(OutputEntry {
                path: rel.to_string_lossy().into_owned(),
                blob,
                mode,
                symlink,
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Restore captured outputs to disk. Returns `false` if any blob is
    /// missing, signalling an incomplete (unusable) cache entry.
    fn restore_outputs(&self, cwd: &Path, outputs: &[OutputEntry]) -> Result<bool> {
        if outputs.iter().any(|e| !self.cas_path(&e.blob).exists()) {
            return Ok(false);
        }
        for entry in outputs {
            let blob_path = self.cas_path(&entry.blob);
            let dest = cwd.join(&entry.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Built beside the destination and renamed over it, so an existing
            // output is replaced rather than written through (a symlink, or a
            // binary that is running).
            let tmp = Self::tmp_path(&dest);
            if entry.symlink {
                Self::make_symlink(&std::fs::read_to_string(&blob_path)?, &tmp)?;
            } else {
                std::fs::copy(&blob_path, &tmp)?;
                if let Some(mode) = entry.mode {
                    Self::set_mode(&tmp, mode)?;
                }
            }
            if let Err(e) = std::fs::rename(&tmp, &dest) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
        Ok(true)
    }

    #[cfg(unix)]
    #[allow(clippy::unnecessary_wraps)] // `None` off unix
    fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777)
    }

    #[cfg(not(unix))]
    const fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
        None
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
        Ok(())
    }

    #[cfg(unix)]
    fn make_symlink(target: &str, link: &Path) -> Result<()> {
        std::os::unix::fs::symlink(target, link)?;
        Ok(())
    }

    // Symlinks are only captured on unix; an entry from a unix remote lands as a copy.
    #[cfg(not(unix))]
    fn make_symlink(target: &str, link: &Path) -> Result<()> {
        let src = link.parent().unwrap_or(Path::new(".")).join(target);
        std::fs::copy(src, link)?;
        Ok(())
    }

    /// Enumerate the concrete files produced by a set of output patterns.
    ///
    /// Unlike source hashing this does **not** consult `.gitignore` — declared
    /// outputs (`target/`, `dist/`, …) are routinely gitignored and must still
    /// be captured. Symlinks below a declared output are kept as symlinks.
    fn collect_output_files(cwd: &Path, patterns: &[String]) -> Vec<PathBuf> {
        fn walk_dir_files(root: &Path, out: &mut Vec<PathBuf>) {
            for entry in WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                let ft = entry.file_type();
                if ft.is_file() || (cfg!(unix) && ft.is_symlink()) {
                    out.push(entry.into_path());
                }
            }
        }

        let mut files = Vec::new();
        for pattern in patterns {
            let (base, literal) = globs::literal_base(pattern);
            let base = cwd.join(base);
            if literal {
                if cfg!(unix) && base.is_symlink() {
                    files.push(base);
                } else if base.is_dir() {
                    walk_dir_files(&base, &mut files);
                } else if base.is_file() {
                    files.push(base);
                }
                continue;
            }
            let Ok(glob) = globs::compile(pattern) else {
                continue;
            };
            let glob = glob.compile_matcher();
            let mut walk = WalkDir::new(&base).follow_links(false).into_iter();
            while let Some(entry) = walk.next() {
                let Ok(entry) = entry else {
                    continue;
                };
                let Ok(rel) = entry.path().strip_prefix(cwd) else {
                    continue;
                };
                if !glob.is_match(rel.to_string_lossy().replace('\\', "/")) {
                    continue;
                }
                let ft = entry.file_type();
                if ft.is_dir() {
                    walk_dir_files(entry.path(), &mut files);
                    walk.skip_current_dir();
                } else if ft.is_file() || (cfg!(unix) && ft.is_symlink()) {
                    files.push(entry.into_path());
                }
            }
        }
        files.sort();
        files.dedup();
        files
    }

    /// Store a blob in the CAS, returning its BLAKE3 digest. Idempotent.
    fn store_blob(&self, content: &[u8]) -> Result<String> {
        let hash = blake3::hash(content).to_hex().to_string();
        let path = self.cas_path(&hash);
        if !path.exists() {
            Self::write_atomic(&path, content)?;
        }
        Ok(hash)
    }

    /// Write a file atomically via a temp file + rename, so concurrent tasks
    /// never observe a half-written blob or action result. The temp name is
    /// unique per call (pid + counter) so concurrent writers of the same target
    /// don't collide on the temp file.
    fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
        let tmp = Self::tmp_path(path);
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn tmp_path(path: &Path) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        path.with_extension(format!("tmp.{}.{n}", std::process::id()))
    }

    /// Path for a local action-cache entry. The cache directory is shared by
    /// every project on the machine, so entries live under a directory per
    /// task directory; only the remote key is shared across checkouts.
    fn ac_path(&self, key: &str, cwd: &Path) -> PathBuf {
        let local = &blake3::hash(key.as_bytes()).to_hex()[..32];
        self.ac_scope(cwd).1.join(format!("{local}.json"))
    }

    /// The canonical task directory and the action-cache directory scoped to it.
    fn ac_scope(&self, cwd: &Path) -> (PathBuf, PathBuf) {
        let dir = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        let scope = &blake3::hash(dir.as_os_str().as_encoded_bytes()).to_hex()[..16];
        let scope_dir = self.dir.join("ac").join(scope);
        (dir, scope_dir)
    }

    /// Write an action-cache entry, recording which directory its scope
    /// belongs to so `yatr cache clear` can find a project's entries.
    fn write_ac(&self, key: &str, cwd: &Path, bytes: &[u8]) -> Result<()> {
        let (dir, scope_dir) = self.ac_scope(cwd);
        let marker = scope_dir.join(SCOPE_MARKER);
        if !marker.exists() {
            std::fs::create_dir_all(&scope_dir)?;
            Self::write_atomic(&marker, dir.to_string_lossy().as_bytes())?;
        }
        Self::write_atomic(&self.ac_path(key, cwd), bytes)
    }

    /// Path for a CAS blob.
    fn cas_path(&self, blob: &str) -> PathBuf {
        self.dir.join("cas").join(blob)
    }
}

/// Cache statistics
#[derive(Debug, Default)]
pub struct CacheStats {
    pub entries: usize,
    pub total_size: u64,
    pub cache_dir: PathBuf,
}

impl std::fmt::Display for CacheStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size_str = if self.total_size < 1024 {
            format!("{} B", self.total_size)
        } else if self.total_size < 1024 * 1024 {
            let kb_int = self.total_size / 1024;
            let kb_frac = (self.total_size % 1024) * 10 / 1024;
            format!("{kb_int}.{kb_frac} KB")
        } else {
            let mb_int = self.total_size / (1024 * 1024);
            let mb_frac = (self.total_size % (1024 * 1024)) * 10 / (1024 * 1024);
            format!("{mb_int}.{mb_frac} MB")
        };

        write!(
            f,
            "{} entries, {} total ({})",
            self.entries,
            size_str,
            self.cache_dir.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(name: &str, config: &TaskConfig, cwd: &Path) -> String {
        Cache::compute_key(name, config, &Config::default(), &[], cwd).unwrap()
    }

    fn task_with(sources: &[&str], outputs: &[&str]) -> TaskConfig {
        let toml = format!(
            "run = [\"true\"]\nsources = [{}]\noutputs = [{}]\n",
            sources
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", "),
            outputs
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(", "),
        );
        toml::from_str(&toml).unwrap()
    }

    #[tokio::test]
    async fn test_cache_put_get_roundtrip() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf())).unwrap();

        let config = task_with(&[], &[]);
        cache
            .put(
                "test",
                &key_of("test", &config, work.path()),
                &config,
                work.path(),
                "hello world",
                Duration::from_millis(5),
            )
            .await
            .unwrap();

        let output = cache
            .get("test", &key_of("test", &config, work.path()), work.path())
            .await
            .unwrap();
        assert_eq!(output, Some("hello world".to_string()));
    }

    #[tokio::test]
    async fn test_outputs_captured_and_restored() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf())).unwrap();

        // Produce an output artifact, then cache it.
        let artifact = work.path().join("dist/app.bin");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, b"compiled bytes").unwrap();

        let config = task_with(&[], &["dist"]);
        cache
            .put(
                "build",
                &key_of("build", &config, work.path()),
                &config,
                work.path(),
                "built",
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        // Delete the artifact — a cache hit must restore it.
        std::fs::remove_dir_all(work.path().join("dist")).unwrap();
        assert!(!artifact.exists());

        let output = cache
            .get("build", &key_of("build", &config, work.path()), work.path())
            .await
            .unwrap();
        assert_eq!(output, Some("built".to_string()));
        assert!(
            artifact.exists(),
            "output should be restored on a cache hit"
        );
        assert_eq!(std::fs::read(&artifact).unwrap(), b"compiled bytes");
    }

    #[tokio::test]
    async fn test_source_change_busts_key() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf())).unwrap();

        let src = work.path().join("input.txt");
        std::fs::write(&src, b"v1").unwrap();

        let config = task_with(&["input.txt"], &[]);
        cache
            .put(
                "t",
                &key_of("t", &config, work.path()),
                &config,
                work.path(),
                "out-v1",
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(
            cache
                .get("t", &key_of("t", &config, work.path()), work.path())
                .await
                .unwrap(),
            Some("out-v1".to_string())
        );

        // Mutating the source must change the key → miss.
        std::fs::write(&src, b"v2").unwrap();
        assert_eq!(
            cache
                .get("t", &key_of("t", &config, work.path()), work.path())
                .await
                .unwrap(),
            None
        );
    }

    /// A `sources` glob under a dot-directory must be hashed like any other.
    /// It previously matched nothing, because the walker skipped hidden
    /// entries, so the task never invalidated when its declared input changed.
    #[tokio::test]
    async fn test_dotted_source_change_busts_key() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf())).unwrap();

        let dir = work.path().join(".config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.json"), b"v1").unwrap();

        let config = task_with(&[".config/f.json"], &[]);
        cache
            .put(
                "t",
                &key_of("t", &config, work.path()),
                &config,
                work.path(),
                "out-v1",
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(
            cache
                .get("t", &key_of("t", &config, work.path()), work.path())
                .await
                .unwrap(),
            Some("out-v1".to_string())
        );

        std::fs::write(dir.join("f.json"), b"v2").unwrap();
        assert_eq!(
            cache
                .get("t", &key_of("t", &config, work.path()), work.path())
                .await
                .unwrap(),
            None,
            "changing a dot-path source must bust the key"
        );
    }

    /// The cache is global, so an unmatched `sources` list used to collapse the
    /// key to task-name-plus-command and serve another project's output.
    #[tokio::test]
    async fn test_dotted_source_does_not_collide_across_projects() {
        let cache_dir = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf())).unwrap();

        for (dir, body) in [(&a, "project-a"), (&b, "project-b")] {
            let d = dir.path().join(".plugin");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("manifest.json"), body.as_bytes()).unwrap();
        }

        let config = task_with(&[".plugin/manifest.json"], &[]);
        cache
            .put(
                "manifest",
                &key_of("manifest", &config, a.path()),
                &config,
                a.path(),
                "out-a",
                Duration::ZERO,
            )
            .await
            .unwrap();

        assert_eq!(
            cache
                .get("manifest", &key_of("manifest", &config, b.path()), b.path())
                .await
                .unwrap(),
            None,
            "project b must not be served project a's cached output"
        );
    }

    /// `.git` must stay out of the hash now that hidden entries are walked:
    /// its contents change on every commit and would destabilise every key.
    #[test]
    fn test_git_directory_is_not_hashed() {
        let work = tempfile::tempdir().unwrap();
        let git = work.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), b"ref: refs/heads/main").unwrap();
        std::fs::write(work.path().join("src.rs"), b"fn main() {}").unwrap();

        let before = Cache::hash_sources(work.path(), &["**/*".to_string()]).unwrap();
        std::fs::write(git.join("HEAD"), b"ref: refs/heads/other").unwrap();
        let after = Cache::hash_sources(work.path(), &["**/*".to_string()]).unwrap();

        assert_eq!(before, after, "a change inside .git must not move the key");
    }

    /// A toolchain pin and a declared `PATH` are key inputs; the bin dirs the
    /// pins install to are machine paths, and are not.
    #[test]
    fn test_toolchain_pin_is_a_key_input() {
        let work = tempfile::tempdir().unwrap();
        let task = task_with(&[], &[]);
        let project = |version: &str, path: &str, bins: &str| -> Config {
            let mut c: Config = toml::from_str(&format!(
                "[env]\nPATH = \"{path}\"\n[toolchain.node]\nversion = \"{version}\"\nurl = \"https://example.com/{{version}}.tgz\"\n"
            ))
            .unwrap();
            c.toolchain_bins = vec![PathBuf::from(bins)];
            c
        };
        let key = |p: &Config| Cache::compute_key("t", &task, p, &[], work.path()).unwrap();

        let base = key(&project("26.11.1", "/usr/bin", "/home/a/tc/bin"));
        assert_ne!(base, key(&project("24.11.1", "/usr/bin", "/home/a/tc/bin")));
        assert_ne!(base, key(&project("26.11.1", "/opt/bin", "/home/a/tc/bin")));
        assert_eq!(base, key(&project("26.11.1", "/usr/bin", "/home/b/tc/bin")));
    }

    #[test]
    fn test_dependency_fingerprint_is_a_key_input() {
        let work = tempfile::tempdir().unwrap();
        let task = task_with(&[], &[]);
        let key = |deps: &[String]| {
            Cache::compute_key("t", &task, &Config::default(), deps, work.path()).unwrap()
        };
        assert_ne!(key(&["a".into()]), key(&["b".into()]));
        assert_eq!(
            key(&["a".into(), "b".into()]),
            key(&["b".into(), "a".into()])
        );
    }

    /// Regression (#47): only the patterns' literal bases are walked.
    #[test]
    fn test_source_walk_stays_inside_literal_bases() {
        let work = tempfile::tempdir().unwrap();
        for dir in ["src/a", "junk/deep", "srcx"] {
            std::fs::create_dir_all(work.path().join(dir)).unwrap();
            std::fs::write(work.path().join(dir).join("f"), b"x").unwrap();
        }
        let bases = [work.path().join("src")];
        let walked: Vec<PathBuf> = Cache::source_walk(work.path(), &bases)
            .filter_map(std::result::Result::ok)
            .map(|e| e.path().strip_prefix(work.path()).unwrap().to_path_buf())
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        assert_eq!(
            walked,
            [
                PathBuf::from("src"),
                PathBuf::from("src/a"),
                PathBuf::from("src/a/f")
            ]
        );
    }

    /// Zero-match source patterns are the symptom `yatr check` should surface.
    #[test]
    fn test_unmatched_source_patterns_are_reported() {
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("real.txt"), b"x").unwrap();
        let dotted = work.path().join(".hidden");
        std::fs::create_dir_all(&dotted).unwrap();
        std::fs::write(dotted.join("also-real.txt"), b"x").unwrap();

        let patterns = vec![
            "real.txt".to_string(),
            ".hidden/also-real.txt".to_string(),
            "nowhere/*.rs".to_string(),
        ];
        let unmatched = Cache::unmatched_source_patterns(work.path(), &patterns).unwrap();
        assert_eq!(unmatched, vec!["nowhere/*.rs".to_string()]);
    }

    #[tokio::test]
    async fn test_clear_task() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf())).unwrap();

        let config = task_with(&[], &[]);
        cache
            .put(
                "a",
                &key_of("a", &config, work.path()),
                &config,
                work.path(),
                "x",
                Duration::ZERO,
            )
            .await
            .unwrap();
        cache
            .put(
                "b",
                &key_of("b", &config, work.path()),
                &config,
                work.path(),
                "y",
                Duration::ZERO,
            )
            .await
            .unwrap();

        assert_eq!(cache.clear_entries(None, Some("a")).unwrap(), 1);
        assert_eq!(
            cache
                .get("a", &key_of("a", &config, work.path()), work.path())
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            cache
                .get("b", &key_of("b", &config, work.path()), work.path())
                .await
                .unwrap(),
            Some("y".to_string())
        );
    }

    fn remote_cfg(url: String) -> crate::config::RemoteCacheConfig {
        crate::config::RemoteCacheConfig {
            url,
            token_env: None,
            sign_key_env: None,
            read: true,
            write: true,
            protocol: CacheProtocol::Native,
        }
    }

    #[tokio::test]
    async fn test_remote_read_through_populates_local() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();

        let config = task_with(&[], &["out.txt"]);
        let key = key_of("build", &config, work.path());
        let blob = blake3::hash(b"remote-bytes").to_hex().to_string();

        let ac = SignedAc {
            sig: None,
            result: ActionResult {
                key: key.clone(),
                task: "build".into(),
                created_at: chrono::Utc::now(),
                duration_ms: 0,
                success: true,
                stdout: "from-remote".into(),
                outputs: vec![OutputEntry {
                    path: "out.txt".into(),
                    blob: blob.clone(),
                    mode: None,
                    symlink: false,
                }],
            },
        };
        let ac_json = serde_json::to_vec(&ac).unwrap();

        Mock::given(method("GET"))
            .and(path(format!("/ac/{key}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(ac_json))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/cas/{blob}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"remote-bytes".to_vec()))
            .mount(&server)
            .await;

        let remote = RemoteCache::from_config(&remote_cfg(server.uri())).unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_remote(Some(remote));

        // Local is empty; the hit must come from the remote and restore the output.
        let out = cache
            .get("build", &key_of("build", &config, work.path()), work.path())
            .await
            .unwrap();
        assert_eq!(out, Some("from-remote".to_string()));
        assert_eq!(
            std::fs::read(work.path().join("out.txt")).unwrap(),
            b"remote-bytes"
        );
        // And the action result is now cached locally for next time.
        assert!(cache.ac_path(&key, work.path()).exists());
    }

    #[tokio::test]
    async fn test_remote_write_through_uploads() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();

        std::fs::write(work.path().join("artifact.bin"), b"payload").unwrap();
        let config = task_with(&[], &["artifact.bin"]);
        let key = key_of("build", &config, work.path());
        let blob = blake3::hash(b"payload").to_hex().to_string();

        // Remote doesn't have the blob yet → expect an upload of blob + action.
        Mock::given(method("HEAD"))
            .and(path(format!("/cas/{blob}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("/cas/{blob}")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("/ac/{key}")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let remote = RemoteCache::from_config(&remote_cfg(server.uri())).unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_remote(Some(remote));

        cache
            .put(
                "build",
                &key_of("build", &config, work.path()),
                &config,
                work.path(),
                "out",
                Duration::from_millis(1),
            )
            .await
            .unwrap();

        // MockServer verifies the expected PUTs were received on drop.
    }

    #[tokio::test]
    async fn test_signed_roundtrip() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let key = Cache::derive_key("super-secret");
        let cache = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_signing_key(Some(key));

        let config = task_with(&[], &[]);
        cache
            .put(
                "t",
                &key_of("t", &config, work.path()),
                &config,
                work.path(),
                "signed-output",
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(
            cache
                .get("t", &key_of("t", &config, work.path()), work.path())
                .await
                .unwrap(),
            Some("signed-output".to_string())
        );
    }

    #[tokio::test]
    async fn test_wrong_key_is_rejected() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let config = task_with(&[], &[]);

        // Written under one key...
        Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_signing_key(Some(Cache::derive_key("key-A")))
            .put(
                "t",
                &key_of("t", &config, work.path()),
                &config,
                work.path(),
                "x",
                Duration::ZERO,
            )
            .await
            .unwrap();

        // ...is rejected when read under a different key (poisoning defence).
        let reader = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_signing_key(Some(Cache::derive_key("key-B")));
        assert_eq!(
            reader
                .get("t", &key_of("t", &config, work.path()), work.path())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_remote_blob_tampering_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();

        let config = task_with(&[], &["out.txt"]);
        let key = key_of("build", &config, work.path());
        let blob = blake3::hash(b"genuine").to_hex().to_string();

        let ac = SignedAc {
            sig: None,
            result: ActionResult {
                key: key.clone(),
                task: "build".into(),
                created_at: chrono::Utc::now(),
                duration_ms: 0,
                success: true,
                stdout: "x".into(),
                outputs: vec![OutputEntry {
                    path: "out.txt".into(),
                    blob: blob.clone(),
                    mode: None,
                    symlink: false,
                }],
            },
        };
        let ac_json = serde_json::to_vec(&ac).unwrap();

        Mock::given(method("GET"))
            .and(path(format!("/ac/{key}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(ac_json))
            .mount(&server)
            .await;
        // Remote serves bytes that do NOT match the advertised digest.
        Mock::given(method("GET"))
            .and(path(format!("/cas/{blob}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"TAMPERED".to_vec()))
            .mount(&server)
            .await;

        let remote = RemoteCache::from_config(&remote_cfg(server.uri())).unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_remote(Some(remote));

        // Integrity check must reject the tampered blob → miss, nothing restored.
        assert_eq!(
            cache
                .get("build", &key_of("build", &config, work.path()), work.path())
                .await
                .unwrap(),
            None
        );
        assert!(!work.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn reapi_fetch_decodes_protobuf_and_restores() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();

        let config = task_with(&[], &["out.txt"]);
        let key = key_of("build", &config, work.path());
        let ac_key = reapi::sha256_hex(key.as_bytes());
        let content = b"reapi bytes";
        let digest = reapi::sha256_hex(content);

        // A protobuf ActionResult, as bazel-remote would store it.
        let ar = reapi::ActionResult {
            output_files: vec![reapi::OutputFile {
                path: "out.txt".into(),
                digest: digest.clone(),
                size: content.len() as u64,
                executable: false,
            }],
            output_symlinks: vec![],
            exit_code: 0,
            stdout: b"from-reapi".to_vec(),
        };

        Mock::given(method("GET"))
            .and(path(format!("/ac/{ac_key}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(reapi::encode_action_result(&ar)),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/cas/{digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&server)
            .await;

        let mut cfg = remote_cfg(server.uri());
        cfg.protocol = CacheProtocol::Reapi;
        let cache = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_remote(Some(RemoteCache::from_config(&cfg).unwrap()));

        let out = cache
            .get("build", &key_of("build", &config, work.path()), work.path())
            .await
            .unwrap();
        assert_eq!(out, Some("from-reapi".to_string()));
        assert_eq!(std::fs::read(work.path().join("out.txt")).unwrap(), content);
    }

    #[tokio::test]
    async fn reapi_upload_uses_sha256_paths() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("art.bin"), b"payload").unwrap();

        let config = task_with(&[], &["art.bin"]);
        let key = key_of("build", &config, work.path());
        let ac_key = reapi::sha256_hex(key.as_bytes());
        let digest = reapi::sha256_hex(b"payload");

        Mock::given(method("HEAD"))
            .and(path(format!("/cas/{digest}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("/cas/{digest}")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(format!("/ac/{ac_key}")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let mut cfg = remote_cfg(server.uri());
        cfg.protocol = CacheProtocol::Reapi;
        let cache = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_remote(Some(RemoteCache::from_config(&cfg).unwrap()));

        cache
            .put(
                "build",
                &key_of("build", &config, work.path()),
                &config,
                work.path(),
                "out",
                Duration::from_millis(1),
            )
            .await
            .unwrap();
        // MockServer verifies the SHA-256-keyed PUTs were received on drop.
    }
}
