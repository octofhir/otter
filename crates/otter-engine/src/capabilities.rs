//! Capability-based security model
//!
//! This module implements a deny-by-default security model where scripts
//! have no permissions unless explicitly granted.
//!
//! # Example
//!
//! ```
//! use otter_engine::capabilities::{Capabilities, CapabilitiesBuilder};
//! use std::path::PathBuf;
//!
//! // Default: everything denied
//! let caps = Capabilities::none();
//! assert!(!caps.can_read("/etc/passwd"));
//!
//! // Grant specific permissions
//! let caps = CapabilitiesBuilder::new()
//!     .allow_read(vec![PathBuf::from("/home/user")])
//!     .allow_net(vec!["api.example.com".into()])
//!     .build();
//!
//! assert!(caps.can_read("/home/user/file.txt"));
//! assert!(!caps.can_read("/etc/passwd"));
//! ```

use std::path::{Path, PathBuf};

/// Capabilities granted to a script execution context.
///
/// By default, all capabilities are denied (None or false).
/// Capabilities must be explicitly granted.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// File system read access
    /// - None: denied
    /// - Some([]): all paths allowed
    /// - Some([paths]): only listed paths (and subdirectories) allowed
    pub fs_read: Option<Vec<PathBuf>>,

    /// File system write access
    /// - None: denied
    /// - Some([]): all paths allowed
    /// - Some([paths]): only listed paths (and subdirectories) allowed
    pub fs_write: Option<Vec<PathBuf>>,

    /// Network access
    /// - None: denied
    /// - Some([]): all hosts allowed
    /// - Some([hosts]): only listed hosts/patterns allowed
    ///   - Exact match: "example.com"
    ///   - Wildcard subdomain: "*.example.com" (matches api.example.com but not example.com)
    pub net: Option<Vec<String>>,

    /// Environment variable access
    /// - None: denied
    /// - Some([]): all env vars allowed
    /// - Some([vars]): only listed vars allowed
    pub env: Option<Vec<String>>,

    /// Subprocess execution
    pub subprocess: bool,

    /// FFI/native code execution
    pub ffi: bool,

    /// High-resolution time (can be used for timing attacks)
    pub hrtime: bool,
}

impl Capabilities {
    /// Create capabilities with everything denied (default).
    pub fn none() -> Self {
        Self::default()
    }

    /// Create capabilities with everything allowed.
    ///
    /// **Warning**: Use with caution, only for trusted scripts.
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

    /// Check path permission against an allowlist.
    fn check_path_permission(&self, allowlist: &Option<Vec<PathBuf>>, path: &Path) -> bool {
        match allowlist {
            None => false,
            Some(allowed) if allowed.is_empty() => true,
            Some(allowed) => {
                // Canonicalize path for comparison if possible
                // Use dunce to avoid Windows extended-length path prefix (\\?\)
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
                    // Wildcard subdomain match
                    let suffix = &pattern[1..]; // ".example.com"
                    host.ends_with(suffix) && host.len() > suffix.len()
                } else {
                    // Exact match
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

    /// Check read permission and return a Result.
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

    /// Check write permission and return a Result.
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

    /// Check network permission and return a Result.
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

    /// Check environment permission and return a Result.
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

    /// Check subprocess permission and return a Result.
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
    /// The capability that was denied (e.g., "read", "write", "net")
    pub capability: String,
    /// The resource that access was denied for
    pub resource: String,
    /// A message explaining how to grant the permission
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
    fn test_none_equals_default() {
        let none = Capabilities::none();
        let default = Capabilities::default();
        assert!(!none.can_read("/any/path"));
        assert!(!default.can_read("/any/path"));
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
    fn test_allow_specific_read_paths() {
        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![PathBuf::from("/home/user")])
            .build();

        assert!(caps.can_read("/home/user"));
        assert!(caps.can_read("/home/user/file.txt"));
        assert!(caps.can_read("/home/user/subdir/file.txt"));
        assert!(!caps.can_read("/etc/passwd"));
        assert!(!caps.can_read("/home/other"));
    }

    #[test]
    fn test_allow_read_all() {
        let caps = CapabilitiesBuilder::new().allow_read_all().build();

        assert!(caps.can_read("/any/path"));
        assert!(caps.can_read("/etc/passwd"));
    }

    #[test]
    fn test_allow_specific_write_paths() {
        // Use current directory which always exists
        let cwd = std::env::current_dir().unwrap();
        let caps = CapabilitiesBuilder::new()
            .allow_write(vec![cwd.clone()])
            .build();

        assert!(caps.can_write(cwd.join("file.txt")));
        assert!(!caps.can_write("/etc/passwd"));
    }

    #[test]
    fn test_wildcard_hosts() {
        let caps = CapabilitiesBuilder::new()
            .allow_net(vec!["*.example.com".into()])
            .build();

        assert!(caps.can_net("api.example.com"));
        assert!(caps.can_net("www.example.com"));
        assert!(caps.can_net("deep.nested.example.com"));
        // Wildcard does not match the base domain
        assert!(!caps.can_net("example.com"));
        assert!(!caps.can_net("other.com"));
    }

    #[test]
    fn test_exact_host_match() {
        let caps = CapabilitiesBuilder::new()
            .allow_net(vec!["example.com".into()])
            .build();

        assert!(caps.can_net("example.com"));
        assert!(!caps.can_net("api.example.com"));
        assert!(!caps.can_net("other.com"));
    }

    #[test]
    fn test_multiple_hosts() {
        let caps = CapabilitiesBuilder::new()
            .allow_net(vec!["api.example.com".into(), "*.internal.io".into()])
            .build();

        assert!(caps.can_net("api.example.com"));
        assert!(caps.can_net("service.internal.io"));
        assert!(!caps.can_net("www.example.com"));
    }

    #[test]
    fn test_allow_net_all() {
        let caps = CapabilitiesBuilder::new().allow_net_all().build();

        assert!(caps.can_net("any.host.com"));
        assert!(caps.can_net("localhost"));
    }

    #[test]
    fn test_specific_env_vars() {
        let caps = CapabilitiesBuilder::new()
            .allow_env(vec!["HOME".into(), "PATH".into()])
            .build();

        assert!(caps.can_env("HOME"));
        assert!(caps.can_env("PATH"));
        assert!(!caps.can_env("SECRET_KEY"));
        assert!(!caps.can_env("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn test_allow_env_all() {
        let caps = CapabilitiesBuilder::new().allow_env_all().build();

        assert!(caps.can_env("HOME"));
        assert!(caps.can_env("SECRET_KEY"));
    }

    #[test]
    fn test_subprocess_permission() {
        let caps_denied = Capabilities::none();
        assert!(!caps_denied.can_subprocess());

        let caps_allowed = CapabilitiesBuilder::new().allow_subprocess().build();
        assert!(caps_allowed.can_subprocess());
    }

    #[test]
    fn test_ffi_permission() {
        let caps_denied = Capabilities::none();
        assert!(!caps_denied.can_ffi());

        let caps_allowed = CapabilitiesBuilder::new().allow_ffi().build();
        assert!(caps_allowed.can_ffi());
    }

    #[test]
    fn test_hrtime_permission() {
        let caps_denied = Capabilities::none();
        assert!(!caps_denied.can_hrtime());

        let caps_allowed = CapabilitiesBuilder::new().allow_hrtime().build();
        assert!(caps_allowed.can_hrtime());
    }

    #[test]
    fn test_require_read_ok() {
        let caps = CapabilitiesBuilder::new().allow_read_all().build();
        assert!(caps.require_read("/any/path").is_ok());
    }

    #[test]
    fn test_require_read_denied() {
        let caps = Capabilities::none();
        let result = caps.require_read("/etc/passwd");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.capability, "read");
        assert!(err.resource.contains("passwd"));
    }

    #[test]
    fn test_require_net_denied() {
        let caps = Capabilities::none();
        let result = caps.require_net("api.example.com");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.capability, "net");
        assert_eq!(err.resource, "api.example.com");
    }

    #[test]
    fn test_permission_denied_display() {
        let err = PermissionDenied::new("read", "/etc/passwd", "Use --allow-read");
        let msg = err.to_string();
        assert!(msg.contains("PermissionDenied"));
        assert!(msg.contains("read"));
        assert!(msg.contains("/etc/passwd"));
    }

    #[test]
    fn test_builder_chaining() {
        // Use current directory which always exists
        let cwd = std::env::current_dir().unwrap();
        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![cwd.clone()])
            .allow_write(vec![cwd.clone()])
            .allow_net(vec!["api.example.com".into()])
            .allow_env(vec!["HOME".into()])
            .allow_subprocess()
            .allow_hrtime()
            .build();

        assert!(caps.can_read(cwd.join("file.txt")));
        assert!(caps.can_write(cwd.join("output.txt")));
        assert!(caps.can_net("api.example.com"));
        assert!(caps.can_env("HOME"));
        assert!(caps.can_subprocess());
        assert!(caps.can_hrtime());
        assert!(!caps.can_ffi()); // Not granted
    }
}
