//! Content-addressable cache for task outputs
//!
//! Uses BLAKE3 hashing to create cache keys based on:
//! - Task configuration
//! - Source file contents
//! - Environment variables
//!
//! Cache entries are stored locally with optional remote sync support planned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use blake3::Hasher;
use globset::{Glob, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::TaskConfig;
use crate::error::{Result, YatrError};

/// Cache entry metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Hash of the cache key
    pub key: String,
    /// Task name
    pub task: String,
    /// Timestamp of creation
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Duration of original execution
    pub duration_ms: u64,
    /// Size of cached output in bytes
    pub output_size: usize,
}

/// Task output cache
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
            directories::ProjectDirs::from("", "", "yatr")
                .map(|d| d.cache_dir().to_path_buf())
                .unwrap_or_else(|| PathBuf::from(".yatr/cache"))
        });

        std::fs::create_dir_all(&dir)?;

        Ok(Self { dir, enabled: true })
    }

    /// Create a disabled cache (no-op)
    pub fn disabled() -> Self {
        Self {
            dir: PathBuf::new(),
            enabled: false,
        }
    }

    /// Check if cache is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get cached output for a task if valid
    pub async fn get(&self, task_name: &str, config: &TaskConfig) -> Result<Option<String>> {
        if !self.enabled {
            return Ok(None);
        }

        let key = self.compute_key(task_name, config).await?;
        let cache_path = self.cache_path(&key);

        if !cache_path.exists() {
            return Ok(None);
        }

        // Read and verify cache entry
        let meta_path = self.meta_path(&key);
        if !meta_path.exists() {
            return Ok(None);
        }

        let meta_content = tokio::fs::read_to_string(&meta_path).await?;
        let entry: CacheEntry = serde_json::from_str(&meta_content).map_err(|_| {
            YatrError::Cache {
                message: "Invalid cache metadata".to_string(),
            }
        })?;

        // Verify the entry is for this task
        if entry.task != task_name {
            return Ok(None);
        }

        // Read cached output
        let output = tokio::fs::read_to_string(&cache_path).await?;
        Ok(Some(output))
    }

    /// Store task output in cache
    pub async fn put(&self, task_name: &str, config: &TaskConfig, output: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let key = self.compute_key(task_name, config).await?;
        let cache_path = self.cache_path(&key);
        let meta_path = self.meta_path(&key);

        // Write output
        tokio::fs::write(&cache_path, output).await?;

        // Write metadata
        let entry = CacheEntry {
            key: key.clone(),
            task: task_name.to_string(),
            created_at: chrono::Utc::now(),
            duration_ms: 0, // TODO: pass actual duration
            output_size: output.len(),
        };

        let meta_content = serde_json::to_string_pretty(&entry).map_err(|e| {
            YatrError::Cache {
                message: format!("Failed to serialize cache metadata: {}", e),
            }
        })?;

        tokio::fs::write(&meta_path, meta_content).await?;

        Ok(())
    }

    /// Invalidate cache for a task
    pub async fn invalidate(&self, task_name: &str, config: &TaskConfig) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let key = self.compute_key(task_name, config).await?;
        let cache_path = self.cache_path(&key);
        let meta_path = self.meta_path(&key);

        if cache_path.exists() {
            tokio::fs::remove_file(&cache_path).await?;
        }

        if meta_path.exists() {
            tokio::fs::remove_file(&meta_path).await?;
        }

        Ok(())
    }

    /// Clear entire cache
    pub async fn clear(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        if self.dir.exists() {
            tokio::fs::remove_dir_all(&self.dir).await?;
            tokio::fs::create_dir_all(&self.dir).await?;
        }

        Ok(())
    }

    /// Get cache statistics
    pub fn stats(&self) -> Result<CacheStats> {
        if !self.enabled {
            return Ok(CacheStats::default());
        }

        let mut total_size = 0u64;
        let mut entry_count = 0usize;

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().map(|e| e == "cache").unwrap_or(false) {
                total_size += entry.metadata()?.len();
                entry_count += 1;
            }
        }

        Ok(CacheStats {
            entries: entry_count,
            total_size,
            cache_dir: self.dir.clone(),
        })
    }

    /// Compute cache key for a task
    async fn compute_key(&self, task_name: &str, config: &TaskConfig) -> Result<String> {
        let mut hasher = Hasher::new();

        // Hash task name
        hasher.update(task_name.as_bytes());

        // Hash commands or script
        for cmd in &config.run {
            hasher.update(cmd.as_bytes());
        }
        if let Some(script) = &config.script {
            hasher.update(script.as_bytes());
        }

        // Hash environment variables (sorted for consistency)
        let mut env_pairs: Vec<_> = config.env.iter().collect();
        env_pairs.sort_by_key(|(k, _)| *k);
        for (k, v) in env_pairs {
            hasher.update(k.as_bytes());
            hasher.update(v.as_bytes());
        }

        // Hash source file contents
        if !config.sources.is_empty() {
            let source_hash = self.hash_sources(&config.sources).await?;
            hasher.update(source_hash.as_bytes());
        }

        let hash = hasher.finalize();
        Ok(hash.to_hex()[..16].to_string())
    }

    /// Hash contents of source files matching glob patterns
    async fn hash_sources(&self, patterns: &[String]) -> Result<String> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|e| YatrError::Cache {
                message: format!("Invalid glob pattern '{}': {}", pattern, e),
            })?;
            builder.add(glob);
        }
        let globset = builder.build().map_err(|e| YatrError::Cache {
            message: format!("Failed to build glob set: {}", e),
        })?;

        let mut hasher = Hasher::new();
        let mut files: Vec<PathBuf> = Vec::new();

        // Collect matching files
        for entry in WalkDir::new(".")
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() && globset.is_match(path) {
                files.push(path.to_path_buf());
            }
        }

        // Sort for consistent ordering
        files.sort();

        // Hash each file
        for path in files {
            hasher.update(path.to_string_lossy().as_bytes());
            let content = tokio::fs::read(&path).await.unwrap_or_default();
            hasher.update(&content);
        }

        let hash = hasher.finalize();
        Ok(hash.to_hex().to_string())
    }

    /// Get path for cache file
    fn cache_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.cache", key))
    }

    /// Get path for metadata file
    fn meta_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}.meta.json", key))
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
            format!("{:.1} KB", self.total_size as f64 / 1024.0)
        } else {
            format!("{:.1} MB", self.total_size as f64 / (1024.0 * 1024.0))
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

    #[tokio::test]
    async fn test_cache_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(Some(temp.path().to_path_buf())).unwrap();

        let config = TaskConfig {
            desc: None,
            run: vec!["echo hello".to_string()],
            script: None,
            depends: vec![],
            parallel: false,
            env: HashMap::new(),
            cwd: None,
            shell: None,
            watch: vec![],
            sources: vec![],
            outputs: vec![],
            no_cache: false,
            allow_failure: false,
            timeout: None,
        };

        // Store in cache
        cache.put("test", &config, "hello world").await.unwrap();

        // Retrieve from cache
        let output = cache.get("test", &config).await.unwrap();
        assert_eq!(output, Some("hello world".to_string()));
    }
}
