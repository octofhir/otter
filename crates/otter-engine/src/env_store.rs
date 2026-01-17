//! Isolated environment store for secure env var access.
//!
//! This module provides a security-first approach to environment variables
//! where JavaScript code cannot directly access host environment variables.
//! Instead, access is controlled through explicit configuration.
//!
//! # Security Model
//!
//! - **Default Deny**: No env vars accessible unless explicitly allowed
//! - **Explicit Vars**: Highest priority, set programmatically
//! - **Passthrough**: Allow specific host vars with deny pattern filtering
//! - **Deny Patterns**: Block sensitive vars even if in passthrough list
//!
//! # Example
//!
//! ```
//! use otter_engine::env_store::{IsolatedEnvStore, EnvStoreBuilder};
//!
//! // Secure by default - nothing accessible
//! let store = IsolatedEnvStore::default();
//! assert!(store.get("HOME").is_none());
//!
//! // Explicit vars only
//! let store = EnvStoreBuilder::new()
//!     .explicit("NODE_ENV", "production")
//!     .explicit("PORT", "3000")
//!     .build();
//! assert_eq!(store.get("NODE_ENV"), Some("production".to_string()));
//! assert!(store.get("AWS_SECRET_KEY").is_none());
//!
//! // Passthrough with deny patterns
//! let store = EnvStoreBuilder::new()
//!     .passthrough(&["HOME", "PATH", "USER"])
//!     .deny_pattern("*_SECRET*")
//!     .build();
//! ```

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Default patterns that block sensitive environment variables.
/// These are always applied unless explicitly disabled.
pub const DEFAULT_DENY_PATTERNS: &[&str] = &[
    // AWS
    "AWS_*",
    "*_AWS_*",
    // Generic secrets
    "*_SECRET*",
    "*_TOKEN*",
    "*_KEY",
    "*_PASSWORD",
    "*_CREDENTIAL*",
    // Database
    "DATABASE_URL",
    "*_DATABASE_*",
    "POSTGRES_*",
    "MYSQL_*",
    "MONGO_*",
    "REDIS_*",
    // API keys
    "*_API_KEY",
    "OPENAI_*",
    "ANTHROPIC_*",
    "STRIPE_*",
    "GITHUB_TOKEN",
    // Auth
    "JWT_*",
    "SESSION_*",
    "COOKIE_*",
    // Private keys
    "*_PRIVATE_*",
    "SSH_*",
];

/// Isolated environment store - JS code CANNOT access host env directly.
///
/// This provides a secure sandbox for environment variable access where
/// only explicitly configured variables are visible to JavaScript code.
#[derive(Debug, Clone)]
pub struct IsolatedEnvStore {
    /// Explicitly set variables (highest priority)
    explicit: HashMap<String, String>,

    /// Allowlist of host env vars to pass through
    passthrough: HashSet<String>,

    /// Denylist patterns (e.g., "*_KEY", "*_SECRET", "*_TOKEN")
    deny_patterns: Vec<String>,

    /// If true, use default deny patterns
    use_default_deny_patterns: bool,

    /// If true, allow setting env vars from JS (default: false)
    allow_write: bool,
}

impl Default for IsolatedEnvStore {
    fn default() -> Self {
        Self {
            explicit: HashMap::new(),
            passthrough: HashSet::new(),
            deny_patterns: Vec::new(),
            use_default_deny_patterns: true,
            allow_write: false,
        }
    }
}

impl IsolatedEnvStore {
    /// Create a new empty store with no access to any env vars.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a store that allows all host env vars (DANGEROUS!).
    ///
    /// **Warning**: This bypasses security protections. Only use for
    /// fully trusted scripts.
    pub fn allow_all() -> Self {
        Self {
            explicit: HashMap::new(),
            passthrough: std::env::vars().map(|(k, _)| k).collect(),
            deny_patterns: Vec::new(),
            use_default_deny_patterns: false,
            allow_write: false,
        }
    }

    /// Get an environment variable value.
    ///
    /// Checks in order:
    /// 1. Explicit vars (highest priority)
    /// 2. Passthrough vars (if allowed and not denied)
    pub fn get(&self, key: &str) -> Option<String> {
        // 1. Check explicit vars first (always allowed)
        if let Some(val) = self.explicit.get(key) {
            return Some(val.clone());
        }

        // 2. Check if in passthrough AND not in deny patterns
        if self.passthrough.contains(key) && !self.is_denied(key) {
            return std::env::var(key).ok();
        }

        None
    }

    /// Set an explicit environment variable.
    ///
    /// Returns `Err` if writes are not allowed.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), EnvWriteError> {
        if !self.allow_write {
            return Err(EnvWriteError::WriteNotAllowed(key.to_string()));
        }
        self.explicit.insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// Remove an explicit environment variable.
    ///
    /// Returns `Err` if writes are not allowed.
    pub fn remove(&mut self, key: &str) -> Result<Option<String>, EnvWriteError> {
        if !self.allow_write {
            return Err(EnvWriteError::WriteNotAllowed(key.to_string()));
        }
        Ok(self.explicit.remove(key))
    }

    /// Check if an environment variable exists and is accessible.
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Get all accessible environment variable keys.
    ///
    /// This is used for `Object.keys(process.env)` in JavaScript.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.explicit.keys().cloned().collect();

        for key in &self.passthrough {
            if !self.is_denied(key) && std::env::var(key).is_ok() && !self.explicit.contains_key(key)
            {
                keys.push(key.clone());
            }
        }

        keys.sort();
        keys
    }

    /// Get all accessible environment variables as a HashMap.
    ///
    /// This is used for `{ ...process.env }` spread in JavaScript.
    pub fn to_hash_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();

        // Add passthrough vars first (lower priority)
        for key in &self.passthrough {
            if !self.is_denied(key) {
                if let Ok(val) = std::env::var(key) {
                    map.insert(key.clone(), val);
                }
            }
        }

        // Add explicit vars (higher priority, overrides passthrough)
        for (key, val) in &self.explicit {
            map.insert(key.clone(), val.clone());
        }

        map
    }

    /// Check if a key is denied by patterns.
    fn is_denied(&self, key: &str) -> bool {
        // Check default patterns if enabled
        if self.use_default_deny_patterns {
            for pattern in DEFAULT_DENY_PATTERNS {
                if matches_pattern(pattern, key) {
                    return true;
                }
            }
        }

        // Check custom deny patterns
        for pattern in &self.deny_patterns {
            if matches_pattern(pattern, key) {
                return true;
            }
        }

        false
    }

    /// Check if a key would be denied (for audit logging).
    pub fn would_deny(&self, key: &str) -> Option<String> {
        // Check default patterns if enabled
        if self.use_default_deny_patterns {
            for pattern in DEFAULT_DENY_PATTERNS {
                if matches_pattern(pattern, key) {
                    return Some((*pattern).to_string());
                }
            }
        }

        // Check custom deny patterns
        for pattern in &self.deny_patterns {
            if matches_pattern(pattern, key) {
                return Some(pattern.clone());
            }
        }

        None
    }

    /// Returns whether writes are allowed.
    pub fn allows_write(&self) -> bool {
        self.allow_write
    }
}

/// Match a glob-like pattern against a key.
///
/// Supports:
/// - `*` at start: matches suffix (e.g., `*_KEY` matches `API_KEY`)
/// - `*` at end: matches prefix (e.g., `AWS_*` matches `AWS_SECRET`)
/// - `*` at both: matches contains (e.g., `*_SECRET*` matches `MY_SECRET_KEY`)
/// - Exact match otherwise
fn matches_pattern(pattern: &str, key: &str) -> bool {
    let starts_with_star = pattern.starts_with('*');
    let ends_with_star = pattern.ends_with('*');

    match (starts_with_star, ends_with_star) {
        (true, true) => {
            // *FOO* - contains match
            let inner = &pattern[1..pattern.len() - 1];
            key.contains(inner)
        }
        (true, false) => {
            // *FOO - suffix match
            let suffix = &pattern[1..];
            key.ends_with(suffix)
        }
        (false, true) => {
            // FOO* - prefix match
            let prefix = &pattern[..pattern.len() - 1];
            key.starts_with(prefix)
        }
        (false, false) => {
            // Exact match
            pattern == key
        }
    }
}

/// Builder for constructing IsolatedEnvStore.
#[derive(Default)]
pub struct EnvStoreBuilder {
    explicit: HashMap<String, String>,
    passthrough: HashSet<String>,
    deny_patterns: Vec<String>,
    use_default_deny_patterns: bool,
    allow_write: bool,
}

impl EnvStoreBuilder {
    /// Create a new builder with secure defaults.
    pub fn new() -> Self {
        Self {
            use_default_deny_patterns: true,
            ..Default::default()
        }
    }

    /// Add an explicit environment variable.
    pub fn explicit(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.explicit.insert(key.into(), value.into());
        self
    }

    /// Add multiple explicit environment variables.
    pub fn explicit_vars(mut self, vars: impl IntoIterator<Item = (String, String)>) -> Self {
        self.explicit.extend(vars);
        self
    }

    /// Add a passthrough variable (from host env).
    pub fn passthrough_var(mut self, var: impl Into<String>) -> Self {
        self.passthrough.insert(var.into());
        self
    }

    /// Add multiple passthrough variables.
    pub fn passthrough(mut self, vars: &[&str]) -> Self {
        for var in vars {
            self.passthrough.insert((*var).to_string());
        }
        self
    }

    /// Add a custom deny pattern.
    pub fn deny_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.deny_patterns.push(pattern.into());
        self
    }

    /// Add multiple deny patterns.
    pub fn deny_patterns(mut self, patterns: &[&str]) -> Self {
        for pattern in patterns {
            self.deny_patterns.push((*pattern).to_string());
        }
        self
    }

    /// Disable default deny patterns (DANGEROUS!).
    pub fn without_default_deny_patterns(mut self) -> Self {
        self.use_default_deny_patterns = false;
        self
    }

    /// Allow writing env vars from JS.
    pub fn allow_write(mut self) -> Self {
        self.allow_write = true;
        self
    }

    /// Load environment variables from a .env file.
    pub fn env_file(mut self, path: impl AsRef<Path>) -> Result<Self, EnvFileError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| EnvFileError::Io {
            path: path.as_ref().to_path_buf(),
            source: e,
        })?;

        let vars = parse_env_file(&content)?;
        self.explicit.extend(vars);
        Ok(self)
    }

    /// Build the IsolatedEnvStore.
    pub fn build(self) -> IsolatedEnvStore {
        IsolatedEnvStore {
            explicit: self.explicit,
            passthrough: self.passthrough,
            deny_patterns: self.deny_patterns,
            use_default_deny_patterns: self.use_default_deny_patterns,
            allow_write: self.allow_write,
        }
    }
}

/// Parse a .env file content into key-value pairs.
///
/// Supports:
/// - Comments starting with `#`
/// - `KEY=VALUE` format
/// - Quoted values (single and double quotes)
/// - `export KEY=VALUE` format (export is ignored)
/// - Multiline values in quotes
pub fn parse_env_file(content: &str) -> Result<HashMap<String, String>, EnvFileError> {
    let mut vars = HashMap::new();
    let mut lines = content.lines().peekable();
    let mut line_num = 0;

    while let Some(line) = lines.next() {
        line_num += 1;
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Strip optional 'export' prefix
        let line = line
            .strip_prefix("export ")
            .or_else(|| line.strip_prefix("export\t"))
            .unwrap_or(line);

        // Parse KEY=VALUE
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();

            // Validate key
            if !is_valid_env_key(key) {
                return Err(EnvFileError::InvalidKey {
                    key: key.to_string(),
                    line: line_num,
                });
            }

            let value = value.trim();
            let parsed_value = parse_env_value(value, &mut lines, &mut line_num)?;
            vars.insert(key.to_string(), parsed_value);
        }
    }

    Ok(vars)
}

/// Parse a value from .env file, handling quotes and multiline.
fn parse_env_value(
    value: &str,
    lines: &mut std::iter::Peekable<std::str::Lines>,
    line_num: &mut usize,
) -> Result<String, EnvFileError> {
    if value.is_empty() {
        return Ok(String::new());
    }

    // Check for quoted values
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        // Simple quoted value on single line
        if value.len() >= 2 {
            return Ok(value[1..value.len() - 1].to_string());
        }
    }

    // Check for multiline quoted value
    if value.starts_with('"') && !value.ends_with('"') {
        let mut multiline = value[1..].to_string();
        while let Some(next_line) = lines.next() {
            *line_num += 1;
            if next_line.ends_with('"') {
                multiline.push('\n');
                multiline.push_str(&next_line[..next_line.len() - 1]);
                return Ok(multiline);
            }
            multiline.push('\n');
            multiline.push_str(next_line);
        }
        return Err(EnvFileError::UnterminatedString { line: *line_num });
    }

    if value.starts_with('\'') && !value.ends_with('\'') {
        let mut multiline = value[1..].to_string();
        while let Some(next_line) = lines.next() {
            *line_num += 1;
            if next_line.ends_with('\'') {
                multiline.push('\n');
                multiline.push_str(&next_line[..next_line.len() - 1]);
                return Ok(multiline);
            }
            multiline.push('\n');
            multiline.push_str(next_line);
        }
        return Err(EnvFileError::UnterminatedString { line: *line_num });
    }

    // Unquoted value
    Ok(value.to_string())
}

/// Check if a string is a valid environment variable key.
fn is_valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !key.chars().next().unwrap().is_ascii_digit()
}

/// Error when writing env vars is not allowed.
#[derive(Debug, Clone)]
pub enum EnvWriteError {
    /// Writing environment variables is not allowed.
    WriteNotAllowed(String),
}

impl std::fmt::Display for EnvWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WriteNotAllowed(key) => {
                write!(
                    f,
                    "Cannot set environment variable '{}': writes not allowed",
                    key
                )
            }
        }
    }
}

impl std::error::Error for EnvWriteError {}

/// Error when parsing .env file.
#[derive(Debug)]
pub enum EnvFileError {
    /// IO error reading file.
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// Invalid environment variable key.
    InvalidKey { key: String, line: usize },
    /// Unterminated quoted string.
    UnterminatedString { line: usize },
}

impl std::fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "Failed to read env file '{}': {}", path.display(), source)
            }
            Self::InvalidKey { key, line } => {
                write!(f, "Invalid environment variable key '{}' at line {}", key, line)
            }
            Self::UnterminatedString { line } => {
                write!(f, "Unterminated quoted string starting at line {}", line)
            }
        }
    }
}

impl std::error::Error for EnvFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_store_denies_all() {
        let store = IsolatedEnvStore::default();
        assert!(store.get("HOME").is_none());
        assert!(store.get("PATH").is_none());
        assert!(store.get("AWS_SECRET_KEY").is_none());
    }

    #[test]
    fn test_explicit_vars() {
        let store = EnvStoreBuilder::new()
            .explicit("NODE_ENV", "production")
            .explicit("PORT", "3000")
            .build();

        assert_eq!(store.get("NODE_ENV"), Some("production".to_string()));
        assert_eq!(store.get("PORT"), Some("3000".to_string()));
        assert!(store.get("HOME").is_none());
    }

    #[test]
    fn test_passthrough_with_deny() {
        // SAFETY: Tests run single-threaded with --test-threads=1
        unsafe {
            // Set a test env var
            std::env::set_var("TEST_PASSTHROUGH_VAR", "test_value");
            std::env::set_var("TEST_SECRET_VAR", "secret_value");
        }

        let store = EnvStoreBuilder::new()
            .passthrough(&["TEST_PASSTHROUGH_VAR", "TEST_SECRET_VAR"])
            .build();

        // Normal var should pass through
        assert_eq!(
            store.get("TEST_PASSTHROUGH_VAR"),
            Some("test_value".to_string())
        );

        // Secret var should be blocked by default deny patterns
        assert!(store.get("TEST_SECRET_VAR").is_none());

        // Clean up
        // SAFETY: Tests run single-threaded
        unsafe {
            std::env::remove_var("TEST_PASSTHROUGH_VAR");
            std::env::remove_var("TEST_SECRET_VAR");
        }
    }

    #[test]
    fn test_keys_returns_only_accessible() {
        let store = EnvStoreBuilder::new()
            .explicit("NODE_ENV", "test")
            .explicit("PORT", "3000")
            .build();

        let keys = store.keys();
        assert!(keys.contains(&"NODE_ENV".to_string()));
        assert!(keys.contains(&"PORT".to_string()));
        assert!(!keys.contains(&"HOME".to_string()));
    }

    #[test]
    fn test_to_hash_map() {
        let store = EnvStoreBuilder::new()
            .explicit("A", "1")
            .explicit("B", "2")
            .build();

        let map = store.to_hash_map();
        assert_eq!(map.get("A"), Some(&"1".to_string()));
        assert_eq!(map.get("B"), Some(&"2".to_string()));
        assert!(map.get("HOME").is_none());
    }

    #[test]
    fn test_deny_patterns() {
        assert!(matches_pattern("*_KEY", "API_KEY"));
        assert!(matches_pattern("*_KEY", "SECRET_KEY"));
        assert!(!matches_pattern("*_KEY", "KEY_VALUE"));

        assert!(matches_pattern("AWS_*", "AWS_SECRET"));
        assert!(matches_pattern("AWS_*", "AWS_ACCESS_KEY_ID"));
        assert!(!matches_pattern("AWS_*", "MY_AWS_VAR"));

        assert!(matches_pattern("*_SECRET*", "MY_SECRET_KEY"));
        assert!(matches_pattern("*_SECRET*", "APP_SECRET_VALUE"));
        assert!(!matches_pattern("*_SECRET*", "SECRET_VALUE")); // No underscore before SECRET

        assert!(matches_pattern("DATABASE_URL", "DATABASE_URL"));
        assert!(!matches_pattern("DATABASE_URL", "MY_DATABASE_URL"));
    }

    #[test]
    fn test_default_deny_patterns_block_secrets() {
        // SAFETY: Tests run single-threaded
        unsafe {
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "secret");
            std::env::set_var("DATABASE_URL", "postgres://...");
            std::env::set_var("GITHUB_TOKEN", "ghp_...");
            std::env::set_var("MY_API_KEY", "key123");
        }

        let store = EnvStoreBuilder::new()
            .passthrough(&[
                "AWS_SECRET_ACCESS_KEY",
                "DATABASE_URL",
                "GITHUB_TOKEN",
                "MY_API_KEY",
            ])
            .build();

        // All should be blocked
        assert!(store.get("AWS_SECRET_ACCESS_KEY").is_none());
        assert!(store.get("DATABASE_URL").is_none());
        assert!(store.get("GITHUB_TOKEN").is_none());
        assert!(store.get("MY_API_KEY").is_none());

        // Clean up
        // SAFETY: Tests run single-threaded
        unsafe {
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
            std::env::remove_var("DATABASE_URL");
            std::env::remove_var("GITHUB_TOKEN");
            std::env::remove_var("MY_API_KEY");
        }
    }

    #[test]
    fn test_explicit_overrides_passthrough() {
        // SAFETY: Tests run single-threaded
        unsafe {
            std::env::set_var("TEST_OVERRIDE_VAR", "from_host");
        }

        let store = EnvStoreBuilder::new()
            .passthrough(&["TEST_OVERRIDE_VAR"])
            .explicit("TEST_OVERRIDE_VAR", "explicit_value")
            .build();

        // Explicit should win
        assert_eq!(
            store.get("TEST_OVERRIDE_VAR"),
            Some("explicit_value".to_string())
        );

        // SAFETY: Tests run single-threaded
        unsafe {
            std::env::remove_var("TEST_OVERRIDE_VAR");
        }
    }

    #[test]
    fn test_write_not_allowed_by_default() {
        let mut store = IsolatedEnvStore::default();
        let result = store.set("FOO", "bar");
        assert!(result.is_err());
    }

    #[test]
    fn test_write_allowed_when_enabled() {
        let mut store = EnvStoreBuilder::new().allow_write().build();
        assert!(store.set("FOO", "bar").is_ok());
        assert_eq!(store.get("FOO"), Some("bar".to_string()));
    }

    #[test]
    fn test_parse_env_file_simple() {
        let content = r#"
# Comment
NODE_ENV=production
PORT=3000
"#;
        let vars = parse_env_file(content).unwrap();
        assert_eq!(vars.get("NODE_ENV"), Some(&"production".to_string()));
        assert_eq!(vars.get("PORT"), Some(&"3000".to_string()));
    }

    #[test]
    fn test_parse_env_file_quoted() {
        let content = r#"
MESSAGE="Hello World"
SINGLE='Single quoted'
"#;
        let vars = parse_env_file(content).unwrap();
        assert_eq!(vars.get("MESSAGE"), Some(&"Hello World".to_string()));
        assert_eq!(vars.get("SINGLE"), Some(&"Single quoted".to_string()));
    }

    #[test]
    fn test_parse_env_file_export() {
        let content = r#"
export DEBUG=true
export VERBOSE=1
"#;
        let vars = parse_env_file(content).unwrap();
        assert_eq!(vars.get("DEBUG"), Some(&"true".to_string()));
        assert_eq!(vars.get("VERBOSE"), Some(&"1".to_string()));
    }

    #[test]
    fn test_parse_env_file_multiline() {
        let content = r#"
MULTILINE="line1
line2
line3"
"#;
        let vars = parse_env_file(content).unwrap();
        assert_eq!(
            vars.get("MULTILINE"),
            Some(&"line1\nline2\nline3".to_string())
        );
    }

    #[test]
    fn test_parse_env_file_empty_value() {
        let content = "EMPTY=\n";
        let vars = parse_env_file(content).unwrap();
        assert_eq!(vars.get("EMPTY"), Some(&"".to_string()));
    }

    #[test]
    fn test_valid_env_key() {
        assert!(is_valid_env_key("NODE_ENV"));
        assert!(is_valid_env_key("MY_VAR_123"));
        assert!(is_valid_env_key("_PRIVATE"));

        assert!(!is_valid_env_key("123_START"));
        assert!(!is_valid_env_key(""));
        assert!(!is_valid_env_key("HAS-DASH"));
        assert!(!is_valid_env_key("HAS.DOT"));
    }

    #[test]
    fn test_would_deny_returns_pattern() {
        let store = IsolatedEnvStore::default();
        assert!(store.would_deny("AWS_SECRET_KEY").is_some());
        assert!(store.would_deny("NORMAL_VAR").is_none());
    }

    #[test]
    fn test_without_default_deny_patterns() {
        // SAFETY: Tests run single-threaded
        unsafe {
            std::env::set_var("TEST_MY_SECRET", "value");
        }

        let store = EnvStoreBuilder::new()
            .without_default_deny_patterns()
            .passthrough(&["TEST_MY_SECRET"])
            .build();

        // Should NOT be blocked without default patterns
        assert_eq!(store.get("TEST_MY_SECRET"), Some("value".to_string()));

        // SAFETY: Tests run single-threaded
        unsafe {
            std::env::remove_var("TEST_MY_SECRET");
        }
    }
}
