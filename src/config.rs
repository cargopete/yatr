//! Configuration parsing for yatr.toml
//!
//! Handles loading and validating the task runner configuration.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Result, YatrError};

/// Default config file names to search for
pub const CONFIG_FILES: &[&str] = &["yatr.toml", "Yatr.toml"];

/// Root configuration structure
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
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
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
}

fn default_true() -> bool {
    true
}

fn default_debounce() -> u64 {
    300
}

/// Configuration for a single task
#[derive(Debug, Clone, Deserialize, Serialize)]
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

        let content = std::fs::read_to_string(&config_path)?;
        let config: Config = toml::from_str(&content).map_err(|e| YatrError::ConfigParse {
            source: e,
            path: config_path.clone(),
        })?;

        config.validate()?;

        Ok((config, config_path))
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
    pub fn get_task(&self, name: &str) -> Option<&TaskConfig> {
        self.tasks.get(name)
    }

    /// List all task names
    pub fn task_names(&self) -> Vec<&str> {
        self.tasks.keys().map(|s| s.as_str()).collect()
    }

    /// Merge environment variables for a task (global + task-specific)
    pub fn task_env(&self, task: &TaskConfig) -> HashMap<String, String> {
        let mut env = self.env.clone();
        env.extend(task.env.clone());
        env
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            env: HashMap::new(),
            tasks: HashMap::new(),
            settings: Settings::default(),
        }
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
}
