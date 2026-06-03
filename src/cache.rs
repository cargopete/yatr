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

/// Task result cache
#[derive(Debug, Clone)]
pub struct Cache {
    /// Cache directory
    dir: PathBuf,
    /// Whether caching is enabled
    enabled: bool,
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

        Ok(Self { dir, enabled: true })
    }

    /// Create a disabled cache (no-op)
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            dir: PathBuf::new(),
            enabled: false,
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
    // Async by design: a remote (REAPI) backend will perform network I/O here.
    #[allow(clippy::unused_async)]
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
        let ac_path = self.ac_path(&key);
        if !ac_path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&ac_path)?;
        let Ok(result) = serde_json::from_str::<ActionResult>(&content) else {
            // Unreadable / stale-format entry: treat as a miss.
            return Ok(None);
        };

        if result.task != task_name || !result.success {
            return Ok(None);
        }

        // Restore declared outputs. If any blob is missing, the cache is
        // incomplete: fall through to a real run rather than lie.
        if !self.restore_outputs(cwd, &result.outputs)? {
            return Ok(None);
        }

        Ok(Some(result.stdout))
    }

    /// Store a successful task result, capturing its declared outputs.
    // Async by design: a remote (REAPI) backend will perform network I/O here.
    #[allow(clippy::unused_async)]
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

        let json = serde_json::to_string_pretty(&result).map_err(|e| YatrError::Cache {
            message: format!("Failed to serialize action result: {e}"),
        })?;

        Self::write_atomic(&self.ac_path(&key), json.as_bytes())
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
            if let Ok(result) = serde_json::from_str::<ActionResult>(&content) {
                if result.task == task_name {
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

        // Working directory and shell mode change command semantics.
        hasher.update(cwd.to_string_lossy().as_bytes());
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
    /// never observe a half-written blob or action result.
    fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
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
}
