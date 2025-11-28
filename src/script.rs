//! Rhai scripting engine integration
//!
//! Provides a sandboxed scripting environment for complex task logic.
//! Rhai was chosen for its fast startup time, Rust-native integration,
//! and familiar syntax.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rhai::{Engine, Scope, AST, Dynamic, EvalAltResult, FuncRegistration, Module};

/// Script execution engine
#[derive(Debug, Clone)]
pub struct ScriptEngine {
    _marker: std::marker::PhantomData<()>,
}

impl ScriptEngine {
    /// Create a new script engine with standard functions registered
    pub fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }

    /// Create a configured engine instance
    fn create_engine() -> Engine {
        let mut engine = Engine::new();

        // Configure sandboxing
        engine.set_max_expr_depths(64, 64);
        engine.set_max_operations(100_000);
        engine.set_max_modules(10);
        engine.set_max_string_size(1024 * 1024); // 1MB

        // Register standard library functions
        Self::register_stdlib(&mut engine);

        engine
    }

    /// Execute a script with the given environment and working directory
    pub fn execute(
        &self,
        script: &str,
        env: &HashMap<String, String>,
        cwd: &Path,
    ) -> Result<String, Box<EvalAltResult>> {
        let mut scope = Scope::new();

        // Inject environment variables
        let env_map: rhai::Map = env
            .iter()
            .map(|(k, v)| (k.clone().into(), Dynamic::from(v.clone())))
            .collect();
        scope.push("env", env_map);

        // Inject working directory
        scope.push("cwd", cwd.to_string_lossy().to_string());

        // Capture output
        let output = Arc::new(std::sync::Mutex::new(String::new()));
        let output_clone = Arc::clone(&output);

        // Create a custom print function that captures output
        let mut engine = Self::create_engine();
        engine.on_print(move |s| {
            let mut out = output_clone.lock().unwrap();
            out.push_str(s);
            out.push('\n');
        });

        // Execute the script
        engine.run_with_scope(&mut scope, script)?;

        let result = output.lock().unwrap().clone();
        Ok(result)
    }

    /// Compile a script for repeated execution
    pub fn compile(&self, script: &str) -> Result<AST, Box<EvalAltResult>> {
        let engine = Self::create_engine();
        engine.compile(script).map_err(|e| e.into())
    }

    /// Execute a pre-compiled script
    pub fn execute_ast(
        &self,
        ast: &AST,
        env: &HashMap<String, String>,
        cwd: &Path,
    ) -> Result<String, Box<EvalAltResult>> {
        let mut scope = Scope::new();

        let env_map: rhai::Map = env
            .iter()
            .map(|(k, v)| (k.clone().into(), Dynamic::from(v.clone())))
            .collect();
        scope.push("env", env_map);
        scope.push("cwd", cwd.to_string_lossy().to_string());

        let output = Arc::new(std::sync::Mutex::new(String::new()));
        let output_clone = Arc::clone(&output);

        let mut engine = Self::create_engine();
        engine.on_print(move |s| {
            let mut out = output_clone.lock().unwrap();
            out.push_str(s);
            out.push('\n');
        });

        engine.run_ast_with_scope(&mut scope, ast)?;

        let result = output.lock().unwrap().clone();
        Ok(result)
    }

    /// Register standard library functions
    fn register_stdlib(engine: &mut Engine) {
        // File operations
        engine.register_fn("read_file", |path: &str| -> Result<String, Box<EvalAltResult>> {
            std::fs::read_to_string(path)
                .map_err(|e| format!("Failed to read file '{}': {}", path, e).into())
        });

        engine.register_fn("write_file", |path: &str, content: &str| -> Result<(), Box<EvalAltResult>> {
            std::fs::write(path, content)
                .map_err(|e| format!("Failed to write file '{}': {}", path, e).into())
        });

        engine.register_fn("file_exists", |path: &str| -> bool {
            std::path::Path::new(path).exists()
        });

        engine.register_fn("is_file", |path: &str| -> bool {
            std::path::Path::new(path).is_file()
        });

        engine.register_fn("is_dir", |path: &str| -> bool {
            std::path::Path::new(path).is_dir()
        });

        // Directory operations
        engine.register_fn("mkdir", |path: &str| -> Result<(), Box<EvalAltResult>> {
            std::fs::create_dir_all(path)
                .map_err(|e| format!("Failed to create directory '{}': {}", path, e).into())
        });

        engine.register_fn("rmdir", |path: &str| -> Result<(), Box<EvalAltResult>> {
            std::fs::remove_dir_all(path)
                .map_err(|e| format!("Failed to remove directory '{}': {}", path, e).into())
        });

        engine.register_fn("list_dir", |path: &str| -> Result<rhai::Array, Box<EvalAltResult>> {
            let entries: Result<Vec<_>, _> = std::fs::read_dir(path)
                .map_err(|e| format!("Failed to read directory '{}': {}", path, e))?
                .map(|e| e.map(|e| Dynamic::from(e.path().to_string_lossy().to_string())))
                .collect();

            entries.map_err(|e: std::io::Error| e.to_string().into())
        });

        // Path operations
        engine.register_fn("join_path", |a: &str, b: &str| -> String {
            std::path::Path::new(a).join(b).to_string_lossy().to_string()
        });

        engine.register_fn("parent_path", |path: &str| -> String {
            std::path::Path::new(path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });

        engine.register_fn("file_name", |path: &str| -> String {
            std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        });

        engine.register_fn("extension", |path: &str| -> String {
            std::path::Path::new(path)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default()
        });

        // Shell command execution
        engine.register_fn("exec", |cmd: &str| -> Result<String, Box<EvalAltResult>> {
            let output = if cfg!(windows) {
                std::process::Command::new("cmd")
                    .args(["/C", cmd])
                    .output()
            } else {
                std::process::Command::new("sh")
                    .args(["-c", cmd])
                    .output()
            };

            match output {
                Ok(o) if o.status.success() => {
                    Ok(String::from_utf8_lossy(&o.stdout).to_string())
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    Err(format!("Command failed: {}", stderr).into())
                }
                Err(e) => Err(format!("Failed to execute command: {}", e).into()),
            }
        });

        // Environment
        engine.register_fn("get_env", |key: &str| -> String {
            std::env::var(key).unwrap_or_default()
        });

        engine.register_fn("set_env", |key: &str, value: &str| {
            std::env::set_var(key, value);
        });

        // String utilities
        engine.register_fn("glob", |pattern: &str| -> Result<rhai::Array, Box<EvalAltResult>> {
            let paths: Vec<_> = glob::glob(pattern)
                .map_err(|e| format!("Invalid glob pattern: {}", e))?
                .filter_map(|p| p.ok())
                .map(|p| Dynamic::from(p.to_string_lossy().to_string()))
                .collect();
            Ok(paths)
        });

        // JSON operations
        engine.register_fn("parse_json", |s: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            let value: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| format!("Failed to parse JSON: {}", e))?;
            json_to_dynamic(value)
        });

        engine.register_fn("to_json", |value: Dynamic| -> Result<String, Box<EvalAltResult>> {
            let json = dynamic_to_json(value)?;
            serde_json::to_string_pretty(&json)
                .map_err(|e| format!("Failed to serialize JSON: {}", e).into())
        });

        // TOML operations
        engine.register_fn("parse_toml", |s: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            let value: toml::Value = toml::from_str(s)
                .map_err(|e| format!("Failed to parse TOML: {}", e))?;
            toml_to_dynamic(value)
        });

        // Version comparison (useful for version bumping)
        engine.register_fn("semver_bump", |version: &str, part: &str| -> Result<String, Box<EvalAltResult>> {
            let parts: Vec<u32> = version
                .split('.')
                .map(|s| s.parse().unwrap_or(0))
                .collect();

            if parts.len() != 3 {
                return Err("Invalid semver format".into());
            }

            let (major, minor, patch) = (parts[0], parts[1], parts[2]);

            let new_version = match part {
                "major" => format!("{}.0.0", major + 1),
                "minor" => format!("{}.{}.0", major, minor + 1),
                "patch" => format!("{}.{}.{}", major, minor, patch + 1),
                _ => return Err(format!("Unknown version part: {}", part).into()),
            };

            Ok(new_version)
        });
    }
}

impl Default for ScriptEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert serde_json::Value to Rhai Dynamic
fn json_to_dynamic(value: serde_json::Value) -> Result<Dynamic, Box<EvalAltResult>> {
    use serde_json::Value;

    Ok(match value {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from(f)
            } else {
                Dynamic::UNIT
            }
        }
        Value::String(s) => Dynamic::from(s),
        Value::Array(arr) => {
            let vec: Result<rhai::Array, _> = arr.into_iter().map(json_to_dynamic).collect();
            Dynamic::from(vec?)
        }
        Value::Object(obj) => {
            let mut map = rhai::Map::new();
            for (k, v) in obj {
                map.insert(k.into(), json_to_dynamic(v)?);
            }
            Dynamic::from(map)
        }
    })
}

/// Convert Rhai Dynamic to serde_json::Value
fn dynamic_to_json(value: Dynamic) -> Result<serde_json::Value, Box<EvalAltResult>> {
    use serde_json::Value;

    if value.is_unit() {
        return Ok(Value::Null);
    }
    if let Some(b) = value.clone().try_cast::<bool>() {
        return Ok(Value::Bool(b));
    }
    if let Some(i) = value.clone().try_cast::<i64>() {
        return Ok(Value::Number(i.into()));
    }
    if let Some(f) = value.clone().try_cast::<f64>() {
        return Ok(serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null));
    }
    if let Some(s) = value.clone().try_cast::<String>() {
        return Ok(Value::String(s));
    }
    if let Some(arr) = value.clone().try_cast::<rhai::Array>() {
        let vec: Result<Vec<_>, _> = arr.into_iter().map(dynamic_to_json).collect();
        return Ok(Value::Array(vec?));
    }
    if let Some(map) = value.clone().try_cast::<rhai::Map>() {
        let obj: Result<serde_json::Map<String, Value>, _> = map
            .into_iter()
            .map(|(k, v)| dynamic_to_json(v).map(|v| (k.to_string(), v)))
            .collect();
        return Ok(Value::Object(obj?));
    }

    Err("Cannot convert value to JSON".into())
}

/// Convert toml::Value to Rhai Dynamic
fn toml_to_dynamic(value: toml::Value) -> Result<Dynamic, Box<EvalAltResult>> {
    use toml::Value;

    Ok(match value {
        Value::Boolean(b) => Dynamic::from(b),
        Value::Integer(i) => Dynamic::from(i),
        Value::Float(f) => Dynamic::from(f),
        Value::String(s) => Dynamic::from(s),
        Value::Datetime(dt) => Dynamic::from(dt.to_string()),
        Value::Array(arr) => {
            let vec: Result<rhai::Array, _> = arr.into_iter().map(toml_to_dynamic).collect();
            Dynamic::from(vec?)
        }
        Value::Table(table) => {
            let mut map = rhai::Map::new();
            for (k, v) in table {
                map.insert(k.into(), toml_to_dynamic(v)?);
            }
            Dynamic::from(map)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_script() {
        let engine = ScriptEngine::new();
        let env = HashMap::new();
        let cwd = std::env::current_dir().unwrap();

        let result = engine.execute(r#"print("Hello, YATR!");"#, &env, &cwd);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "Hello, YATR!");
    }

    #[test]
    fn test_env_access() {
        let engine = ScriptEngine::new();
        let mut env = HashMap::new();
        env.insert("MY_VAR".to_string(), "test_value".to_string());
        let cwd = std::env::current_dir().unwrap();

        let result = engine.execute(r#"print(env["MY_VAR"]);"#, &env, &cwd);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "test_value");
    }

    #[test]
    fn test_semver_bump() {
        let engine = ScriptEngine::new();
        let env = HashMap::new();
        let cwd = std::env::current_dir().unwrap();

        let result = engine.execute(
            r#"let v = semver_bump("1.2.3", "minor"); print(v);"#,
            &env,
            &cwd,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "1.3.0");
    }
}
