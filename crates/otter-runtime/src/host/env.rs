use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Default patterns that block sensitive environment variables.
pub const DEFAULT_DENY_PATTERNS: &[&str] = &[
    "AWS_*",
    "*_AWS_*",
    "*_SECRET*",
    "*_TOKEN*",
    "*_KEY",
    "*_PASSWORD",
    "*_CREDENTIAL*",
    "DATABASE_URL",
    "*_DATABASE_*",
    "POSTGRES_*",
    "MYSQL_*",
    "MONGO_*",
    "REDIS_*",
    "*_API_KEY",
    "OPENAI_*",
    "ANTHROPIC_*",
    "STRIPE_*",
    "GITHUB_TOKEN",
    "JWT_*",
    "SESSION_*",
    "COOKIE_*",
    "*_PRIVATE_*",
    "SSH_*",
];

/// Isolated environment store for secure env var access.
#[derive(Debug, Clone)]
pub struct IsolatedEnvStore {
    explicit: HashMap<String, String>,
    passthrough: HashSet<String>,
    deny_patterns: Vec<String>,
    use_default_deny_patterns: bool,
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

    /// Create a store that allows all host env vars.
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
    pub fn get(&self, key: &str) -> Option<String> {
        if let Some(val) = self.explicit.get(key) {
            return Some(val.clone());
        }

        if self.passthrough.contains(key) && !self.is_denied(key) {
            return std::env::var(key).ok();
        }

        None
    }

    /// Set an explicit environment variable.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), EnvWriteError> {
        if !self.allow_write {
            return Err(EnvWriteError::WriteNotAllowed(key.to_string()));
        }
        self.explicit.insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// Remove an explicit environment variable.
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
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.explicit.keys().cloned().collect();

        for key in &self.passthrough {
            if !self.is_denied(key)
                && std::env::var(key).is_ok()
                && !self.explicit.contains_key(key)
            {
                keys.push(key.clone());
            }
        }

        keys.sort();
        keys
    }

    /// Get all accessible environment variables as a map.
    pub fn to_hash_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();

        for key in &self.passthrough {
            if !self.is_denied(key)
                && let Ok(val) = std::env::var(key)
            {
                map.insert(key.clone(), val);
            }
        }

        for (key, val) in &self.explicit {
            map.insert(key.clone(), val.clone());
        }

        map
    }

    fn is_denied(&self, key: &str) -> bool {
        if self.use_default_deny_patterns {
            for pattern in DEFAULT_DENY_PATTERNS {
                if matches_pattern(pattern, key) {
                    return true;
                }
            }
        }

        for pattern in &self.deny_patterns {
            if matches_pattern(pattern, key) {
                return true;
            }
        }

        false
    }

    /// Returns the deny pattern that blocked access, if any.
    pub fn would_deny(&self, key: &str) -> Option<String> {
        if self.use_default_deny_patterns {
            for pattern in DEFAULT_DENY_PATTERNS {
                if matches_pattern(pattern, key) {
                    return Some((*pattern).to_string());
                }
            }
        }

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

fn matches_pattern(pattern: &str, key: &str) -> bool {
    let starts_with_star = pattern.starts_with('*');
    let ends_with_star = pattern.ends_with('*');

    match (starts_with_star, ends_with_star) {
        (true, true) => {
            let inner = &pattern[1..pattern.len() - 1];
            key.contains(inner)
        }
        (true, false) => {
            let suffix = &pattern[1..];
            key.ends_with(suffix)
        }
        (false, true) => {
            let prefix = &pattern[..pattern.len() - 1];
            key.starts_with(prefix)
        }
        (false, false) => pattern == key,
    }
}

/// Builder for constructing `IsolatedEnvStore`.
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

    /// Add a passthrough variable.
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

    /// Disable default deny patterns.
    pub fn without_default_deny_patterns(mut self) -> Self {
        self.use_default_deny_patterns = false;
        self
    }

    /// Allow writing env vars from JS.
    pub fn allow_write(mut self) -> Self {
        self.allow_write = true;
        self
    }

    /// Load environment variables from a `.env` file.
    pub fn env_file(mut self, path: impl AsRef<Path>) -> Result<Self, EnvFileError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| EnvFileError::Io {
            path: path.as_ref().to_path_buf(),
            source: e,
        })?;

        let vars = parse_env_file(&content)?;
        self.explicit.extend(vars);
        Ok(self)
    }

    /// Build the store.
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

/// Parse a `.env` file content into key-value pairs.
pub fn parse_env_file(content: &str) -> Result<HashMap<String, String>, EnvFileError> {
    let mut vars = HashMap::new();
    let mut lines = content.lines().peekable();
    let mut line_num = 0;

    while let Some(line) = lines.next() {
        line_num += 1;
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line
            .strip_prefix("export ")
            .or_else(|| line.strip_prefix("export\t"))
            .unwrap_or(line);

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();

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

fn parse_env_value(
    value: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
    line_num: &mut usize,
) -> Result<String, EnvFileError> {
    if value.is_empty() {
        return Ok(String::new());
    }

    if ((value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\'')))
        && value.len() >= 2
    {
        return Ok(value[1..value.len() - 1].to_string());
    }

    if value.starts_with('"') && !value.ends_with('"') {
        let mut multiline = value[1..].to_string();
        for next_line in lines.by_ref() {
            *line_num += 1;
            if next_line.ends_with('"') {
                multiline.push('\n');
                if let Some(stripped) = next_line.strip_suffix('"') {
                    multiline.push_str(stripped);
                }
                return Ok(multiline);
            }
            multiline.push('\n');
            multiline.push_str(next_line);
        }
        return Err(EnvFileError::UnterminatedString { line: *line_num });
    }

    if value.starts_with('\'') && !value.ends_with('\'') {
        let mut multiline = value[1..].to_string();
        for next_line in lines.by_ref() {
            *line_num += 1;
            if next_line.ends_with('\'') {
                multiline.push('\n');
                if let Some(stripped) = next_line.strip_suffix('\'') {
                    multiline.push_str(stripped);
                }
                return Ok(multiline);
            }
            multiline.push('\n');
            multiline.push_str(next_line);
        }
        return Err(EnvFileError::UnterminatedString { line: *line_num });
    }

    Ok(value.to_string())
}

fn is_valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !key.chars().next().expect("non-empty key").is_ascii_digit()
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

/// Error when parsing `.env` files.
#[derive(Debug)]
pub enum EnvFileError {
    /// IO error reading file.
    Io {
        /// Path to the file that could not be read.
        path: std::path::PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// Invalid environment variable key.
    InvalidKey {
        /// The invalid key that was encountered.
        key: String,
        /// Line number where the invalid key was found.
        line: usize,
    },
    /// Unterminated quoted string.
    UnterminatedString {
        /// Line number where the unterminated string started.
        line: usize,
    },
}

impl std::fmt::Display for EnvFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    f,
                    "Failed to read env file '{}': {}",
                    path.display(),
                    source
                )
            }
            Self::InvalidKey { key, line } => {
                write!(
                    f,
                    "Invalid environment variable key '{}' at line {}",
                    key, line
                )
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
