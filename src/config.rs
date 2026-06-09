//! Configuration parsing for yatr.toml
//!
//! Handles loading and validating the task runner configuration.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Result, YatrError};

/// Default config file names to search for
pub const CONFIG_FILES: &[&str] = &["yatr.toml", "Yatr.toml"];

/// Root configuration structure
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    /// Other yatr.toml files to merge in (paths relative to this file).
    /// Their tasks and env are composed into this config; their settings are
    /// ignored (the root file's settings are authoritative).
    #[serde(default)]
    pub include: Vec<PathBuf>,

    /// Global environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Task definitions
    #[serde(default)]
    pub tasks: HashMap<String, TaskConfig>,

    /// Global settings
    #[serde(default)]
    pub settings: Settings,
}

/// Global settings for YATR behavior
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Default shell to use (if shell mode enabled)
    #[serde(default)]
    pub shell: Option<String>,

    /// Enable caching by default
    #[serde(default = "default_true")]
    pub cache: bool,

    /// Cache directory (defaults to .yatr/cache)
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,

    /// Default parallelism level (0 = number of CPUs)
    #[serde(default)]
    pub parallelism: usize,

    /// Watch debounce delay in milliseconds
    #[serde(default = "default_debounce")]
    pub watch_debounce_ms: u64,

    /// Optional shared/remote cache backend
    #[serde(default)]
    pub remote_cache: Option<RemoteCacheConfig>,
}

/// Configuration for a shared HTTP remote cache.
///
/// The cache speaks a simple REST protocol — `GET`/`PUT`/`HEAD` on
/// `<url>/ac/<key>` (action results) and `<url>/cas/<blob>` (content blobs) —
/// so it works against a plain object store or a small server, and shares the
/// path layout used by Bazel's HTTP cache.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteCacheConfig {
    /// Base URL of the remote cache (e.g. `https://cache.example.com/yatr`)
    pub url: String,

    /// Name of the environment variable holding a bearer token, if auth is
    /// required. Keeps secrets out of the committed config file.
    #[serde(default)]
    pub token_env: Option<String>,

    /// Name of the environment variable holding a shared secret used to sign
    /// and verify cached action results (keyed BLAKE3 MAC). When set, entries
    /// that fail verification are rejected — protection against cache poisoning.
    #[serde(default)]
    pub sign_key_env: Option<String>,

    /// Read from the remote cache on a local miss
    #[serde(default = "default_true")]
    pub read: bool,

    /// Write to the remote cache after a successful run
    #[serde(default = "default_true")]
    pub write: bool,
}

const fn default_true() -> bool {
    true
}

const fn default_debounce() -> u64 {
    300
}

/// Configuration for a single task
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    /// Human-readable description
    #[serde(default)]
    pub desc: Option<String>,

    /// Commands to run (simple string list mode)
    #[serde(default)]
    pub run: Vec<String>,

    /// Rhai script to execute (alternative to `run`)
    #[serde(default)]
    pub script: Option<String>,

    /// Tasks that must complete before this one
    #[serde(default)]
    pub depends: Vec<String>,

    /// Run commands in parallel
    #[serde(default)]
    pub parallel: bool,

    /// Task-specific environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Working directory for this task
    #[serde(default)]
    pub cwd: Option<PathBuf>,

    /// Use shell to execute commands
    #[serde(default)]
    pub shell: Option<bool>,

    /// Run in foreground with inherited stdio (for long-running processes like dev servers)
    #[serde(default)]
    pub foreground: bool,

    /// Files to watch for this task (glob patterns)
    #[serde(default)]
    pub watch: Vec<String>,

    /// Files that affect the cache key (glob patterns)
    #[serde(default)]
    pub sources: Vec<String>,

    /// Output files/directories produced by this task
    #[serde(default)]
    pub outputs: Vec<String>,

    /// Skip caching for this task
    #[serde(default)]
    pub no_cache: bool,

    /// Continue even if this task fails
    #[serde(default)]
    pub allow_failure: bool,

    /// Timeout in seconds
    #[serde(default)]
    pub timeout: Option<u64>,
}

impl Config {
    /// Load configuration from the specified path or search for it
    pub fn load(path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let config_path = match path {
            Some(p) => {
                if p.exists() {
                    p.to_path_buf()
                } else {
                    return Err(YatrError::ConfigNotFound {
                        searched: vec![p.to_path_buf()],
                    });
                }
            }
            None => Self::find_config()?,
        };

        let mut visited = std::collections::HashSet::new();
        let config = Self::load_with_includes(&config_path, &mut visited)?;
        config.validate()?;

        Ok((config, config_path))
    }

    /// Load a config file and recursively merge any files it `include`s.
    fn load_with_includes(
        path: &Path,
        visited: &mut std::collections::HashSet<PathBuf>,
    ) -> Result<Self> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !visited.insert(canonical) {
            return Err(YatrError::InvalidConfig {
                message: format!("include cycle detected at {}", path.display()),
            });
        }

        let content = std::fs::read_to_string(path).map_err(|e| YatrError::InvalidConfig {
            message: format!("failed to read included config {}: {e}", path.display()),
        })?;
        let mut config: Self = toml::from_str(&content).map_err(|e| YatrError::ConfigParse {
            source: e,
            path: path.to_path_buf(),
        })?;

        let base = path.parent().unwrap_or_else(|| Path::new("."));
        for inc in std::mem::take(&mut config.include) {
            let inc_path = base.join(&inc);
            let included = Self::load_with_includes(&inc_path, visited)?;
            config.merge_from(included)?;
        }

        Ok(config)
    }

    /// Merge another config's tasks and env into this one. Duplicate task names
    /// are an error; existing (root) env entries win over included ones; the
    /// other config's settings are ignored.
    fn merge_from(&mut self, other: Self) -> Result<()> {
        for (name, task) in other.tasks {
            if self.tasks.contains_key(&name) {
                return Err(YatrError::InvalidConfig {
                    message: format!("task '{name}' is defined in more than one config file"),
                });
            }
            self.tasks.insert(name, task);
        }
        for (key, value) in other.env {
            self.env.entry(key).or_insert(value);
        }
        Ok(())
    }

    /// Search for config file starting from current directory
    fn find_config() -> Result<PathBuf> {
        let mut current = std::env::current_dir()?;
        let mut searched = Vec::new();

        loop {
            for name in CONFIG_FILES {
                let candidate = current.join(name);
                searched.push(candidate.clone());
                if candidate.exists() {
                    return Ok(candidate);
                }
            }

            if !current.pop() {
                break;
            }
        }

        Err(YatrError::ConfigNotFound { searched })
    }

    /// Validate the configuration
    fn validate(&self) -> Result<()> {
        for (name, task) in &self.tasks {
            // Task must have either `run`, `script`, or dependencies
            let has_run = !task.run.is_empty();
            let has_script = task.script.is_some();
            let has_depends = !task.depends.is_empty();

            if !has_run && !has_script && !has_depends {
                return Err(YatrError::InvalidTask {
                    task: name.clone(),
                    reason: "Task must have 'run' commands, 'script', or 'depends'".to_string(),
                });
            }

            if has_run && has_script {
                return Err(YatrError::InvalidTask {
                    task: name.clone(),
                    reason: "Task cannot have both 'run' and 'script'".to_string(),
                });
            }

            // Check for self-dependency
            if task.depends.contains(name) {
                return Err(YatrError::InvalidTask {
                    task: name.clone(),
                    reason: "Task cannot depend on itself".to_string(),
                });
            }
        }

        Ok(())
    }

    /// Get a task by name
    #[must_use]
    pub fn get_task(&self, name: &str) -> Option<&TaskConfig> {
        self.tasks.get(name)
    }

    /// List all task names
    #[must_use]
    pub fn task_names(&self) -> Vec<&str> {
        self.tasks.keys().map(std::string::String::as_str).collect()
    }

    /// Merge environment variables for a task (global + task-specific)
    #[must_use]
    pub fn task_env(&self, task: &TaskConfig) -> HashMap<String, String> {
        let mut env = self.env.clone();
        env.extend(task.env.clone());
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_config() {
        let toml = r#"
            [env]
            RUST_LOG = "debug"

            [tasks.test]
            desc = "Run tests"
            run = ["cargo test"]

            [tasks.build]
            desc = "Build release"
            depends = ["test"]
            run = ["cargo build --release"]
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.env.get("RUST_LOG"), Some(&"debug".to_string()));
        assert_eq!(config.tasks.len(), 2);
        assert!(config.tasks.contains_key("test"));
        assert!(config.tasks.contains_key("build"));
    }

    #[test]
    fn test_parse_parallel_task() {
        let toml = r#"
            [tasks.lint]
            parallel = true
            run = ["cargo fmt --check", "cargo clippy"]
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.tasks["lint"].parallel);
    }

    #[test]
    fn test_parse_script_task() {
        let toml = r#"
            [tasks.bump]
            desc = "Bump version"
            script = '''
                let version = "0.2.0";
                print(`New version: ${version}`);
            '''
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.tasks["bump"].script.is_some());
    }

    #[test]
    fn test_include_merges_tasks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shared.toml"),
            "[tasks.lint]\nrun=[\"echo lint\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("yatr.toml"),
            "include=[\"shared.toml\"]\n[tasks.build]\nrun=[\"echo build\"]\n",
        )
        .unwrap();

        let (config, _) = Config::load(Some(&dir.path().join("yatr.toml"))).unwrap();
        assert!(config.tasks.contains_key("build"));
        assert!(config.tasks.contains_key("lint"));
    }

    #[test]
    fn test_include_duplicate_task_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shared.toml"),
            "[tasks.build]\nrun=[\"echo a\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("yatr.toml"),
            "include=[\"shared.toml\"]\n[tasks.build]\nrun=[\"echo b\"]\n",
        )
        .unwrap();

        assert!(Config::load(Some(&dir.path().join("yatr.toml"))).is_err());
    }

    #[test]
    fn test_include_cycle_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.toml"),
            "include=[\"b.toml\"]\n[tasks.x]\nrun=[\"echo x\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.toml"),
            "include=[\"a.toml\"]\n[tasks.y]\nrun=[\"echo y\"]\n",
        )
        .unwrap();

        assert!(Config::load(Some(&dir.path().join("a.toml"))).is_err());
    }
}
