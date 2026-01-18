//! Script runner for package.json scripts.
//!
//! Provides functionality to run npm scripts with proper PATH setup,
//! lifecycle hooks (pre/post), and fuzzy matching for typo suggestions.

use crate::install::PackageJson;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use strsim::levenshtein;

/// Script execution result
#[derive(Debug)]
pub struct ScriptResult {
    pub status: ExitStatus,
    pub script_name: String,
}

/// Error type for script operations
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("Script '{0}' not found in package.json")]
    NotFound(String),

    #[error("No package.json found in {0}")]
    NoPackageJson(PathBuf),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("Script '{0}' failed with exit code {1}")]
    Failed(String, i32),
}

/// Script runner with lifecycle hooks support
pub struct ScriptRunner {
    project_dir: PathBuf,
    scripts: HashMap<String, String>,
    bin_path: PathBuf,
}

impl ScriptRunner {
    /// Create a new ScriptRunner by loading package.json from the given directory
    pub fn new(project_dir: &Path) -> Result<Self, ScriptError> {
        let pkg_json_path = project_dir.join("package.json");
        if !pkg_json_path.exists() {
            return Err(ScriptError::NoPackageJson(project_dir.to_path_buf()));
        }

        let content = std::fs::read_to_string(&pkg_json_path)?;
        let pkg: PackageJson = serde_json::from_str(&content)?;

        let scripts = pkg.scripts.unwrap_or_default();
        let bin_path = project_dir.join("node_modules/.bin");

        Ok(Self {
            project_dir: project_dir.to_path_buf(),
            scripts,
            bin_path,
        })
    }

    /// Try to create a ScriptRunner, returns None if no package.json exists
    pub fn try_new(project_dir: &Path) -> Option<Self> {
        Self::new(project_dir).ok()
    }

    /// Check if a script exists
    pub fn has_script(&self, name: &str) -> bool {
        self.scripts.contains_key(name)
    }

    /// Get a script command by name
    pub fn get_script(&self, name: &str) -> Option<&String> {
        self.scripts.get(name)
    }

    /// List all available scripts
    pub fn list(&self) -> Vec<(&str, &str)> {
        let mut scripts: Vec<_> = self
            .scripts
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        scripts.sort_by(|a, b| a.0.cmp(b.0));
        scripts
    }

    /// Get script names only
    pub fn script_names(&self) -> Vec<&str> {
        self.scripts.keys().map(|s| s.as_str()).collect()
    }

    /// Find similar script names for typo suggestions (Levenshtein distance ≤ 2)
    pub fn suggest(&self, typo: &str) -> Vec<(&str, &str)> {
        let mut suggestions: Vec<_> = self
            .scripts
            .iter()
            .filter_map(|(name, cmd)| {
                let distance = levenshtein(typo, name);
                if distance <= 2 {
                    Some((distance, name.as_str(), cmd.as_str()))
                } else {
                    None
                }
            })
            .collect();

        // Sort by distance (closest first)
        suggestions.sort_by_key(|(d, _, _)| *d);
        suggestions
            .into_iter()
            .map(|(_, name, cmd)| (name, cmd))
            .collect()
    }

    /// Build PATH with node_modules/.bin prepended
    fn build_path(&self) -> String {
        let mut paths = Vec::new();

        // Local node_modules/.bin (highest priority)
        if self.bin_path.exists() {
            paths.push(self.bin_path.display().to_string());
        }

        // Walk up directory tree for nested node_modules
        let mut current = self.project_dir.parent();
        while let Some(dir) = current {
            let bin = dir.join("node_modules/.bin");
            if bin.exists() {
                paths.push(bin.display().to_string());
            }
            current = dir.parent();
        }

        // Existing PATH
        if let Ok(existing) = std::env::var("PATH") {
            paths.push(existing);
        }

        paths.join(":")
    }

    /// Run a script with lifecycle hooks (pre/post)
    pub fn run(&self, name: &str, args: &[String]) -> Result<ScriptResult, ScriptError> {
        let script = self
            .scripts
            .get(name)
            .ok_or_else(|| ScriptError::NotFound(name.to_string()))?;

        // Run pre-hook if exists
        let pre_name = format!("pre{}", name);
        if self.scripts.contains_key(&pre_name) {
            self.run_single(&pre_name, &[])?;
        }

        // Run main script
        let result = self.run_single_with_args(name, script, args)?;

        // Run post-hook if exists (only if main succeeded)
        if result.status.success() {
            let post_name = format!("post{}", name);
            if self.scripts.contains_key(&post_name) {
                self.run_single(&post_name, &[])?;
            }
        }

        Ok(result)
    }

    /// Run a single script without hooks
    fn run_single(&self, name: &str, args: &[String]) -> Result<ScriptResult, ScriptError> {
        let script = self
            .scripts
            .get(name)
            .ok_or_else(|| ScriptError::NotFound(name.to_string()))?;
        self.run_single_with_args(name, script, args)
    }

    /// Run a script command with arguments
    fn run_single_with_args(
        &self,
        name: &str,
        script: &str,
        args: &[String],
    ) -> Result<ScriptResult, ScriptError> {
        // Build full command with args
        let full_cmd = if args.is_empty() {
            script.to_string()
        } else {
            format!("{} {}", script, shell_escape_args(args))
        };

        let path = self.build_path();

        let status = Command::new("sh")
            .arg("-c")
            .arg(&full_cmd)
            .current_dir(&self.project_dir)
            .env("PATH", &path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;

        Ok(ScriptResult {
            status,
            script_name: name.to_string(),
        })
    }

    /// Get the project directory
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }
}

/// Escape arguments for shell
fn shell_escape_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(' ') || arg.contains('"') || arg.contains('\'') {
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format scripts for display
pub fn format_scripts_list(scripts: &[(&str, &str)]) -> String {
    if scripts.is_empty() {
        return String::from("  (no scripts defined)");
    }

    let max_name_len = scripts.iter().map(|(name, _)| name.len()).max().unwrap_or(0);

    scripts
        .iter()
        .map(|(name, cmd)| {
            let truncated_cmd = if cmd.len() > 50 {
                format!("{}...", &cmd[..47])
            } else {
                cmd.to_string()
            };
            format!("  {:<width$} → {}", name, truncated_cmd, width = max_name_len)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find package.json by walking up from the given path
pub fn find_package_json(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()
    } else {
        Some(start)
    };

    while let Some(dir) = current {
        let pkg_json = dir.join("package.json");
        if pkg_json.exists() {
            return Some(pkg_json);
        }
        current = dir.parent();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_project(scripts: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let mut scripts_map: HashMap<String, String> = HashMap::new();
        for (name, cmd) in scripts {
            scripts_map.insert(name.to_string(), cmd.to_string());
        }

        let pkg = PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: None,
            dev_dependencies: None,
            scripts: Some(scripts_map),
            bin: None,
            main: None,
            pkg_type: None,
        };

        let content = serde_json::to_string_pretty(&pkg).unwrap();
        fs::write(dir.path().join("package.json"), content).unwrap();
        dir
    }

    #[test]
    fn test_script_runner_new() {
        let dir = create_test_project(&[("build", "echo building"), ("test", "echo testing")]);
        let runner = ScriptRunner::new(dir.path()).unwrap();

        assert!(runner.has_script("build"));
        assert!(runner.has_script("test"));
        assert!(!runner.has_script("nonexistent"));
    }

    #[test]
    fn test_script_list() {
        let dir = create_test_project(&[("build", "echo build"), ("test", "echo test")]);
        let runner = ScriptRunner::new(dir.path()).unwrap();
        let list = runner.list();

        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_suggest_typos() {
        let dir = create_test_project(&[
            ("build", "echo build"),
            ("test", "echo test"),
            ("dev", "echo dev"),
        ]);
        let runner = ScriptRunner::new(dir.path()).unwrap();

        // "biuld" should suggest "build" (distance = 2)
        let suggestions = runner.suggest("biuld");
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].0, "build");

        // "tset" should suggest "test" (distance = 2)
        let suggestions = runner.suggest("tset");
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].0, "test");

        // "xyz" should not match anything
        let suggestions = runner.suggest("xyz");
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_run_simple_script() {
        let dir = create_test_project(&[("hello", "echo hello")]);
        let runner = ScriptRunner::new(dir.path()).unwrap();

        let result = runner.run("hello", &[]).unwrap();
        assert!(result.status.success());
    }

    #[test]
    fn test_format_scripts_list() {
        let scripts = vec![("build", "tsc"), ("test", "jest --coverage")];
        let output = format_scripts_list(&scripts);

        assert!(output.contains("build"));
        assert!(output.contains("tsc"));
        assert!(output.contains("test"));
    }
}
