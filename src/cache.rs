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
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use blake3::Hasher;
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::TaskConfig;
use crate::error::{Result, YatrError};
use crate::remote::RemoteCache;

/// A single cached output file: its path relative to the task's working
/// directory, and the BLAKE3 digest of its contents in the CAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputEntry {
    pub path: String,
    pub blob: String,
}

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

    /// Look up a cached result for a task and, on a hit, restore its outputs.
    ///
    /// Returns `None` (a miss) when there is no entry, the entry is for a
    /// different task, or any recorded output blob is missing — in which case
    /// the caller should run the task for real.
    pub async fn get(
        &self,
        task_name: &str,
        config: &TaskConfig,
        cwd: &Path,
    ) -> Result<Option<String>> {
        if !self.enabled {
            return Ok(None);
        }

        let key = Self::compute_key(task_name, config, cwd)?;

        // Local action cache first.
        let result = if let Some(result) = self.load_local_ac(&key, task_name) {
            Some(result)
        } else {
            // Local miss: try the remote, populating the local cache on a hit.
            self.fetch_from_remote(&key, task_name).await
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
    fn load_local_ac(&self, key: &str, task_name: &str) -> Option<ActionResult> {
        let bytes = std::fs::read(self.ac_path(key)).ok()?;
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
    async fn fetch_from_remote(&self, key: &str, task_name: &str) -> Option<ActionResult> {
        let remote = self.remote.as_ref()?;
        if !remote.read {
            return None;
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
        Self::write_atomic(&self.ac_path(key), &ac_bytes).ok()?;
        Some(result)
    }

    /// Store a successful task result, capturing its declared outputs.
    pub async fn put(
        &self,
        task_name: &str,
        config: &TaskConfig,
        cwd: &Path,
        stdout: &str,
        duration: Duration,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let key = Self::compute_key(task_name, config, cwd)?;
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

        Self::write_atomic(&self.ac_path(&key), &bytes)?;

        // Write-through to the remote (non-fatal).
        self.upload_to_remote(&key, &signed.result, bytes).await;
        Ok(())
    }

    fn ser_err(e: &serde_json::Error) -> YatrError {
        YatrError::Cache {
            message: format!("Failed to serialize action result: {e}"),
        }
    }

    /// Upload an action result and its blobs to the remote cache. Best-effort:
    /// any failure is logged and swallowed so it can't break the build.
    async fn upload_to_remote(&self, key: &str, result: &ActionResult, ac_bytes: Vec<u8>) {
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        if !remote.write {
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

    /// Invalidate the cached entry for a specific task + input combination.
    // Async by design: a remote (REAPI) backend will perform network I/O here.
    #[allow(clippy::unused_async)]
    pub async fn invalidate(&self, task_name: &str, config: &TaskConfig, cwd: &Path) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let key = Self::compute_key(task_name, config, cwd)?;
        let ac_path = self.ac_path(&key);
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

    /// Clear all action-cache entries for a named task, regardless of inputs.
    ///
    /// Returns the number of entries removed. Orphaned CAS blobs are left in
    /// place (cheap, content-addressed, and reused by other entries); a full
    /// `clear` reclaims them.
    pub fn clear_task(&self, task_name: &str) -> Result<usize> {
        if !self.enabled {
            return Ok(0);
        }

        let ac_dir = self.dir.join("ac");
        if !ac_dir.exists() {
            return Ok(0);
        }

        let mut removed = 0;
        for entry in std::fs::read_dir(&ac_dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(signed) = serde_json::from_str::<SignedAc>(&content) {
                if signed.result.task == task_name {
                    std::fs::remove_file(&path)?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Get cache statistics
    pub fn stats(&self) -> Result<CacheStats> {
        if !self.enabled {
            return Ok(CacheStats::default());
        }

        let mut total_size = 0u64;
        let mut entry_count = 0usize;

        let ac_dir = self.dir.join("ac");
        if ac_dir.exists() {
            for entry in std::fs::read_dir(&ac_dir)? {
                let entry = entry?;
                if entry.path().extension().is_some_and(|e| e == "json") {
                    total_size += entry.metadata()?.len();
                    entry_count += 1;
                }
            }
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

    /// Compute the cache key for a task.
    fn compute_key(task_name: &str, config: &TaskConfig, cwd: &Path) -> Result<String> {
        let mut hasher = Hasher::new();

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

        // Environment variables (sorted for stability).
        let mut env_pairs: Vec<_> = config.env.iter().collect();
        env_pairs.sort_by_key(|(k, _)| *k);
        for (k, v) in env_pairs {
            hasher.update(k.as_bytes());
            hasher.update(v.as_bytes());
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

    /// Hash the contents of source files matching the glob patterns, rooted at
    /// `cwd` and respecting `.gitignore` (so build artifacts and `node_modules`
    /// don't bloat or destabilise the key).
    fn hash_sources(cwd: &Path, patterns: &[String]) -> Result<String> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|e| YatrError::Cache {
                message: format!("Invalid glob pattern '{pattern}': {e}"),
            })?;
            builder.add(glob);
        }
        let globset = builder.build().map_err(|e| YatrError::Cache {
            message: format!("Failed to build glob set: {e}"),
        })?;

        // Collect matching files relative to cwd. The `ignore` walker skips
        // .git and honours .gitignore; it does not follow symlinks by default.
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        for entry in WalkBuilder::new(cwd)
            .build()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(cwd) else {
                continue;
            };
            if globset.is_match(rel) {
                files.push((rel.to_string_lossy().into_owned(), path.to_path_buf()));
            }
        }

        // Sort by relative path for a deterministic hash.
        files.sort_by(|a, b| a.0.cmp(&b.0));

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
            let content = std::fs::read(&path)?;
            let blob = self.store_blob(&content)?;
            entries.push(OutputEntry {
                path: rel.to_string_lossy().into_owned(),
                blob,
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    /// Restore captured outputs to disk. Returns `false` if any blob is
    /// missing, signalling an incomplete (unusable) cache entry.
    fn restore_outputs(&self, cwd: &Path, outputs: &[OutputEntry]) -> Result<bool> {
        for entry in outputs {
            let blob_path = self.cas_path(&entry.blob);
            if !blob_path.exists() {
                return Ok(false);
            }
            let dest = cwd.join(&entry.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&blob_path, &dest)?;
        }
        Ok(true)
    }

    /// Enumerate the concrete files produced by a set of output patterns.
    ///
    /// Unlike source hashing this does **not** consult `.gitignore` — declared
    /// outputs (`target/`, `dist/`, …) are routinely gitignored and must still
    /// be captured.
    fn collect_output_files(cwd: &Path, patterns: &[String]) -> Vec<PathBuf> {
        fn walk_dir_files(root: &Path, out: &mut Vec<PathBuf>) {
            for entry in WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                if entry.file_type().is_file() {
                    out.push(entry.into_path());
                }
            }
        }

        let mut files = Vec::new();
        for pattern in patterns {
            let full = cwd.join(pattern);
            if full.is_dir() {
                walk_dir_files(&full, &mut files);
            } else if let Ok(paths) = glob::glob(&full.to_string_lossy()) {
                for p in paths.filter_map(std::result::Result::ok) {
                    if p.is_dir() {
                        walk_dir_files(&p, &mut files);
                    } else if p.is_file() {
                        files.push(p);
                    }
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
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{}.{n}", std::process::id()));
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Path for an action-cache entry.
    fn ac_path(&self, key: &str) -> PathBuf {
        self.dir.join("ac").join(format!("{key}.json"))
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
                &config,
                work.path(),
                "hello world",
                Duration::from_millis(5),
            )
            .await
            .unwrap();

        let output = cache.get("test", &config, work.path()).await.unwrap();
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

        let output = cache.get("build", &config, work.path()).await.unwrap();
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
            .put("t", &config, work.path(), "out-v1", Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(
            cache.get("t", &config, work.path()).await.unwrap(),
            Some("out-v1".to_string())
        );

        // Mutating the source must change the key → miss.
        std::fs::write(&src, b"v2").unwrap();
        assert_eq!(cache.get("t", &config, work.path()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_clear_task() {
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(cache_dir.path().to_path_buf())).unwrap();

        let config = task_with(&[], &[]);
        cache
            .put("a", &config, work.path(), "x", Duration::ZERO)
            .await
            .unwrap();
        cache
            .put("b", &config, work.path(), "y", Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(cache.clear_task("a").unwrap(), 1);
        assert_eq!(cache.get("a", &config, work.path()).await.unwrap(), None);
        assert_eq!(
            cache.get("b", &config, work.path()).await.unwrap(),
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
        let key = Cache::compute_key("build", &config, work.path()).unwrap();
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
        let out = cache.get("build", &config, work.path()).await.unwrap();
        assert_eq!(out, Some("from-remote".to_string()));
        assert_eq!(
            std::fs::read(work.path().join("out.txt")).unwrap(),
            b"remote-bytes"
        );
        // And the action result is now cached locally for next time.
        assert!(cache.ac_path(&key).exists());
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
        let key = Cache::compute_key("build", &config, work.path()).unwrap();
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
            .put("t", &config, work.path(), "signed-output", Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(
            cache.get("t", &config, work.path()).await.unwrap(),
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
            .put("t", &config, work.path(), "x", Duration::ZERO)
            .await
            .unwrap();

        // ...is rejected when read under a different key (poisoning defence).
        let reader = Cache::new(Some(cache_dir.path().to_path_buf()))
            .unwrap()
            .with_signing_key(Some(Cache::derive_key("key-B")));
        assert_eq!(reader.get("t", &config, work.path()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_remote_blob_tampering_rejected() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let cache_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();

        let config = task_with(&[], &["out.txt"]);
        let key = Cache::compute_key("build", &config, work.path()).unwrap();
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
            cache.get("build", &config, work.path()).await.unwrap(),
            None
        );
        assert!(!work.path().join("out.txt").exists());
    }
}
