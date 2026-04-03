use std::path::{Path, PathBuf};

/// Capabilities granted to a script execution context.
///
/// By default, all capabilities are denied.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// File system read access.
    pub fs_read: Option<Vec<PathBuf>>,
    /// File system write access.
    pub fs_write: Option<Vec<PathBuf>>,
    /// Network access.
    pub net: Option<Vec<String>>,
    /// Environment variable access.
    pub env: Option<Vec<String>>,
    /// Subprocess execution.
    pub subprocess: bool,
    /// FFI/native code execution.
    pub ffi: bool,
    /// High-resolution time.
    pub hrtime: bool,
}

impl Capabilities {
    /// Create capabilities with everything denied.
    pub fn none() -> Self {
        Self::default()
    }

    /// Create capabilities with everything allowed.
    ///
    /// Use only for trusted scripts.
    pub fn all() -> Self {
        Self {
            fs_read: Some(Vec::new()),
            fs_write: Some(Vec::new()),
            net: Some(Vec::new()),
            env: Some(Vec::new()),
            subprocess: true,
            ffi: true,
            hrtime: true,
        }
    }

    /// Check if file read is allowed for a path.
    pub fn can_read<P: AsRef<Path>>(&self, path: P) -> bool {
        self.check_path_permission(&self.fs_read, path.as_ref())
    }

    /// Check if file write is allowed for a path.
    pub fn can_write<P: AsRef<Path>>(&self, path: P) -> bool {
        self.check_path_permission(&self.fs_write, path.as_ref())
    }

    fn check_path_permission(&self, allowlist: &Option<Vec<PathBuf>>, path: &Path) -> bool {
        match allowlist {
            None => false,
            Some(allowed) if allowed.is_empty() => true,
            Some(allowed) => {
                let path = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                allowed.iter().any(|allowed_path| {
                    let allowed =
                        dunce::canonicalize(allowed_path).unwrap_or_else(|_| allowed_path.clone());
                    path.starts_with(&allowed)
                })
            }
        }
    }

    /// Check if network access is allowed for a host.
    pub fn can_net(&self, host: &str) -> bool {
        match &self.net {
            None => false,
            Some(allowed) if allowed.is_empty() => true,
            Some(allowed) => allowed.iter().any(|pattern| {
                if pattern.starts_with("*.") {
                    let suffix = &pattern[1..];
                    host.ends_with(suffix) && host.len() > suffix.len()
                } else {
                    pattern == host
                }
            }),
        }
    }

    /// Check if environment variable access is allowed.
    pub fn can_env(&self, var: &str) -> bool {
        match &self.env {
            None => false,
            Some(allowed) if allowed.is_empty() => true,
            Some(allowed) => allowed.iter().any(|v| v == var),
        }
    }

    /// Check if subprocess execution is allowed.
    pub fn can_subprocess(&self) -> bool {
        self.subprocess
    }

    /// Check if FFI is allowed.
    pub fn can_ffi(&self) -> bool {
        self.ffi
    }

    /// Check if high-resolution time is allowed.
    pub fn can_hrtime(&self) -> bool {
        self.hrtime
    }

    /// Check read permission and return a `Result`.
    pub fn require_read<P: AsRef<Path>>(&self, path: P) -> Result<(), PermissionDenied> {
        let path = path.as_ref();
        if self.can_read(path) {
            Ok(())
        } else {
            Err(PermissionDenied::new(
                "read",
                path.display().to_string(),
                "Use --allow-read to grant file system read access",
            ))
        }
    }

    /// Check write permission and return a `Result`.
    pub fn require_write<P: AsRef<Path>>(&self, path: P) -> Result<(), PermissionDenied> {
        let path = path.as_ref();
        if self.can_write(path) {
            Ok(())
        } else {
            Err(PermissionDenied::new(
                "write",
                path.display().to_string(),
                "Use --allow-write to grant file system write access",
            ))
        }
    }

    /// Check network permission and return a `Result`.
    pub fn require_net(&self, host: &str) -> Result<(), PermissionDenied> {
        if self.can_net(host) {
            Ok(())
        } else {
            Err(PermissionDenied::new(
                "net",
                host.to_string(),
                "Use --allow-net to grant network access",
            ))
        }
    }

    /// Check environment permission and return a `Result`.
    pub fn require_env(&self, var: &str) -> Result<(), PermissionDenied> {
        if self.can_env(var) {
            Ok(())
        } else {
            Err(PermissionDenied::new(
                "env",
                var.to_string(),
                "Use --allow-env to grant environment variable access",
            ))
        }
    }

    /// Check subprocess permission and return a `Result`.
    pub fn require_subprocess(&self) -> Result<(), PermissionDenied> {
        if self.can_subprocess() {
            Ok(())
        } else {
            Err(PermissionDenied::new(
                "run",
                "subprocess".to_string(),
                "Use --allow-run to grant subprocess execution",
            ))
        }
    }

    /// Check FFI permission and return a `Result`.
    pub fn require_ffi(&self) -> Result<(), PermissionDenied> {
        if self.can_ffi() {
            Ok(())
        } else {
            Err(PermissionDenied::new(
                "ffi",
                "native".to_string(),
                "Use --allow-ffi to grant FFI access",
            ))
        }
    }
}

/// Builder for constructing capabilities.
#[derive(Default)]
pub struct CapabilitiesBuilder {
    caps: Capabilities,
}

impl CapabilitiesBuilder {
    /// Create a new builder with all permissions denied.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allow reading from specific paths.
    pub fn allow_read(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let paths: Vec<_> = paths.into_iter().collect();
        self.caps.fs_read = Some(paths);
        self
    }

    /// Allow reading from all paths.
    pub fn allow_read_all(mut self) -> Self {
        self.caps.fs_read = Some(Vec::new());
        self
    }

    /// Allow writing to specific paths.
    pub fn allow_write(mut self, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let paths: Vec<_> = paths.into_iter().collect();
        self.caps.fs_write = Some(paths);
        self
    }

    /// Allow writing to all paths.
    pub fn allow_write_all(mut self) -> Self {
        self.caps.fs_write = Some(Vec::new());
        self
    }

    /// Allow network access to specific hosts.
    pub fn allow_net(mut self, hosts: impl IntoIterator<Item = String>) -> Self {
        let hosts: Vec<_> = hosts.into_iter().collect();
        self.caps.net = Some(hosts);
        self
    }

    /// Allow network access to all hosts.
    pub fn allow_net_all(mut self) -> Self {
        self.caps.net = Some(Vec::new());
        self
    }

    /// Allow reading specific environment variables.
    pub fn allow_env(mut self, vars: impl IntoIterator<Item = String>) -> Self {
        let vars: Vec<_> = vars.into_iter().collect();
        self.caps.env = Some(vars);
        self
    }

    /// Allow reading all environment variables.
    pub fn allow_env_all(mut self) -> Self {
        self.caps.env = Some(Vec::new());
        self
    }

    /// Allow subprocess execution.
    pub fn allow_subprocess(mut self) -> Self {
        self.caps.subprocess = true;
        self
    }

    /// Allow FFI.
    pub fn allow_ffi(mut self) -> Self {
        self.caps.ffi = true;
        self
    }

    /// Allow high-resolution time.
    pub fn allow_hrtime(mut self) -> Self {
        self.caps.hrtime = true;
        self
    }

    /// Build the capabilities.
    pub fn build(self) -> Capabilities {
        self.caps
    }
}

/// Error returned when a capability check fails.
#[derive(Debug, Clone)]
pub struct PermissionDenied {
    /// The capability that was denied.
    pub capability: String,
    /// The resource that access was denied for.
    pub resource: String,
    /// A message explaining how to grant the permission.
    pub message: String,
}

impl PermissionDenied {
    /// Create a new permission denied error.
    pub fn new(
        capability: impl Into<String>,
        resource: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            capability: capability.into(),
            resource: resource.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PermissionDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PermissionDenied: {} access to '{}'. {}",
            self.capability, self.resource, self.message
        )
    }
}

impl std::error::Error for PermissionDenied {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_denies_all() {
        let caps = Capabilities::default();
        assert!(!caps.can_read("/etc/passwd"));
        assert!(!caps.can_write("/tmp/test"));
        assert!(!caps.can_net("example.com"));
        assert!(!caps.can_env("HOME"));
        assert!(!caps.can_subprocess());
        assert!(!caps.can_ffi());
        assert!(!caps.can_hrtime());
    }

    #[test]
    fn test_all_allows_everything() {
        let caps = Capabilities::all();
        assert!(caps.can_read("/etc/passwd"));
        assert!(caps.can_write("/tmp/test"));
        assert!(caps.can_net("example.com"));
        assert!(caps.can_env("HOME"));
        assert!(caps.can_subprocess());
        assert!(caps.can_ffi());
        assert!(caps.can_hrtime());
    }

    #[test]
    fn test_wildcard_hosts() {
        let caps = CapabilitiesBuilder::new()
            .allow_net(vec!["*.example.com".into()])
            .build();

        assert!(caps.can_net("api.example.com"));
        assert!(!caps.can_net("example.com"));
    }

    #[test]
    fn test_permission_denied_display() {
        let err = PermissionDenied::new("read", "/etc/passwd", "Use --allow-read");
        let msg = err.to_string();
        assert!(msg.contains("PermissionDenied"));
        assert!(msg.contains("read"));
        assert!(msg.contains("/etc/passwd"));
    }
}
