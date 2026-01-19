//! Node.js `process` module implementation.
//!
//! Provides process-related information and utilities with secure
//! environment variable access through `IsolatedEnvStore`.
//!
//! # Security
//!
//! The `process.env` object is initialized from `IsolatedEnvStore` which:
//! - Blocks access to sensitive env vars by default (AWS keys, tokens, etc.)
//! - Only exposes explicitly configured variables
//! - Script-local writes are allowed (standard Node.js behavior)
//! - Writes don't affect the host environment
//!
//! # Example
//!
//! ```javascript
//! // Only sees vars from IsolatedEnvStore, not host env
//! console.log(process.env.NODE_ENV);  // "production" (if configured)
//! console.log(process.env.AWS_SECRET); // undefined (blocked)
//!
//! // Process info
//! console.log(process.platform);  // "darwin"
//! console.log(process.arch);      // "arm64"
//! console.log(process.pid);       // 12345
//! ```

use otter_engine::IsolatedEnvStore;
use std::sync::Arc;

/// Process information and utilities.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    /// Environment store for secure env var access
    pub env_store: Arc<IsolatedEnvStore>,

    /// Command line arguments
    pub argv: Vec<String>,

    /// Current working directory
    pub cwd: String,

    /// Process ID
    pub pid: u32,

    /// Parent process ID
    pub ppid: u32,

    /// Platform identifier
    pub platform: &'static str,

    /// Architecture identifier
    pub arch: &'static str,

    /// Otter version (presented as Node.js version for compatibility)
    pub version: String,
}

impl Default for ProcessInfo {
    fn default() -> Self {
        Self::new(Arc::new(IsolatedEnvStore::default()), vec![])
    }
}

impl ProcessInfo {
    /// Create process info with the given env store and argv.
    pub fn new(env_store: Arc<IsolatedEnvStore>, argv: Vec<String>) -> Self {
        Self {
            env_store,
            argv,
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string()),
            pid: std::process::id(),
            ppid: get_ppid(),
            platform: get_platform(),
            arch: get_arch(),
            version: format!("v{}", env!("CARGO_PKG_VERSION")),
        }
    }

    /// Get environment variable through the isolated store.
    pub fn get_env(&self, key: &str) -> Option<String> {
        self.env_store.get(key)
    }

    /// Get all accessible environment variable keys.
    pub fn env_keys(&self) -> Vec<String> {
        self.env_store.keys()
    }

    /// Get all accessible environment variables.
    pub fn env_to_object(&self) -> std::collections::HashMap<String, String> {
        self.env_store.to_hash_map()
    }

    /// Generate JavaScript code to set up the process global.
    pub fn to_js_setup(&self) -> String {
        let env_json = serde_json::to_string(&self.env_to_object()).unwrap_or_else(|_| "{}".into());
        let argv_json = serde_json::to_string(&self.argv).unwrap_or_else(|_| "[]".into());

        format!(
            r#"
(function() {{
    const envData = {env_json};

    // Create a Proxy for process.env that returns undefined for non-existent keys
    // and prevents enumeration of host environment
    const envProxy = new Proxy(envData, {{
        get(target, prop) {{
            if (typeof prop === 'string') {{
                return target[prop];
            }}
            return undefined;
        }},
        set(target, prop, value) {{
            // Allow writes (standard Node.js behavior, writes are script-local)
            if (typeof prop === 'string') {{
                target[prop] = String(value);
            }}
            return true;
        }},
        deleteProperty(target, prop) {{
            // Allow deletes (standard Node.js behavior)
            if (typeof prop === 'string') {{
                delete target[prop];
            }}
            return true;
        }},
        has(target, prop) {{
            return typeof prop === 'string' && prop in target;
        }},
        ownKeys(target) {{
            return Object.keys(target);
        }},
        getOwnPropertyDescriptor(target, prop) {{
            if (prop in target) {{
                return {{
                    value: target[prop],
                    writable: false,
                    enumerable: true,
                    configurable: true  // Must be configurable for Proxy invariants
                }};
            }}
            return undefined;
        }}
    }});

    globalThis.process = {{
        env: envProxy,
        argv: {argv_json},
        cwd: () => {cwd:?},
        chdir: (dir) => {{ throw new Error('process.chdir() is not supported in Otter'); }},
        exit: (code) => {{ throw new Error('process.exit() called with code ' + (code || 0)); }},
        pid: {pid},
        ppid: {ppid},
        platform: {platform:?},
        arch: {arch:?},
        version: {version:?},
        versions: {{
            otter: {version:?},
            node: {version:?},
            jsc: 'unknown'
        }},
        // Stub implementations
        nextTick: (callback, ...args) => {{
            queueMicrotask(() => callback(...args));
        }},
        hrtime: {{
            bigint: () => BigInt(Math.floor(performance.now() * 1000000))
        }},
        memoryUsage: () => {{
            if (typeof __otter_process_memory_usage === 'function') {{
                return __otter_process_memory_usage();
            }}
            return {{
                rss: 0,
                heapTotal: 0,
                heapUsed: 0,
                external: 0,
                arrayBuffers: 0
            }};
        }},
        // Stubs for compatibility
        stdin: null,
        stdout: {{ write: (s) => console.log(s) }},
        stderr: {{ write: (s) => console.error(s) }},

        // EventEmitter methods (will be enhanced by IPC extension if available)
        _listeners: {{}},
        on: function(event, handler) {{
            if (!this._listeners[event]) this._listeners[event] = [];
            this._listeners[event].push(handler);
            return this;
        }},
        off: function(event, handler) {{
            if (this._listeners[event]) {{
                this._listeners[event] = this._listeners[event].filter(h => h !== handler);
            }}
            return this;
        }},
        once: function(event, handler) {{
            const wrapper = (...args) => {{
                this.off(event, wrapper);
                handler(...args);
            }};
            return this.on(event, wrapper);
        }},
        emit: function(event, ...args) {{
            if (this._listeners[event]) {{
                this._listeners[event].forEach(h => h(...args));
            }}
            return this._listeners[event]?.length > 0;
        }},
        removeListener: function(event, handler) {{
            return this.off(event, handler);
        }},
        removeAllListeners: function(event) {{
            if (event) {{
                delete this._listeners[event];
            }} else {{
                this._listeners = {{}};
            }}
            return this;
        }},
        listeners: function(event) {{
            return this._listeners[event] || [];
        }},
        listenerCount: function(event) {{
            return this._listeners[event]?.length || 0;
        }},

        // IPC methods (stubs - will be overridden by IPC extension if available)
        connected: false,
        send: function(message, callback) {{
            if (typeof __otter_process_ipc_send === 'function') {{
                return __otter_process_ipc_send(message, callback);
            }}
            throw new Error('process.send() is only available in forked processes');
        }},
        disconnect: function() {{
            if (typeof __otter_process_ipc_disconnect === 'function') {{
                return __otter_process_ipc_disconnect();
            }}
        }}
    }};

    // Expose as a Node.js builtin module when module system is present.
    if (globalThis.__registerNodeBuiltin) {{
        globalThis.__registerNodeBuiltin('process', globalThis.process);
    }}
}})();
"#,
            env_json = env_json,
            argv_json = argv_json,
            cwd = self.cwd,
            pid = self.pid,
            ppid = self.ppid,
            platform = self.platform,
            arch = self.arch,
            version = self.version,
        )
    }
}

/// Get the current platform identifier.
fn get_platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "win32"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "unknown"
    }
}

/// Get the current architecture identifier.
fn get_arch() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86")]
    {
        "ia32"
    }
    #[cfg(target_arch = "arm")]
    {
        "arm"
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "arm"
    )))]
    {
        "unknown"
    }
}

/// Get the parent process ID.
#[cfg(unix)]
fn get_ppid() -> u32 {
    // SAFETY: getppid() is always safe to call
    unsafe { libc::getppid() as u32 }
}

#[cfg(not(unix))]
fn get_ppid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_engine::EnvStoreBuilder;

    #[test]
    fn test_process_info_defaults() {
        let info = ProcessInfo::default();
        assert!(info.pid > 0);
        assert!(!info.platform.is_empty());
        assert!(!info.arch.is_empty());
    }

    #[test]
    fn test_process_env_isolation() {
        let env_store = Arc::new(
            EnvStoreBuilder::new()
                .explicit("NODE_ENV", "test")
                .explicit("PORT", "3000")
                .build(),
        );

        let info = ProcessInfo::new(env_store, vec!["otter".into(), "app.ts".into()]);

        assert_eq!(info.get_env("NODE_ENV"), Some("test".to_string()));
        assert_eq!(info.get_env("PORT"), Some("3000".to_string()));
        assert!(info.get_env("AWS_SECRET_KEY").is_none());
    }

    #[test]
    fn test_process_env_keys() {
        let env_store = Arc::new(
            EnvStoreBuilder::new()
                .explicit("A", "1")
                .explicit("B", "2")
                .build(),
        );

        let info = ProcessInfo::new(env_store, vec![]);
        let keys = info.env_keys();

        assert!(keys.contains(&"A".to_string()));
        assert!(keys.contains(&"B".to_string()));
        assert!(!keys.contains(&"HOME".to_string()));
    }

    #[test]
    fn test_to_js_setup() {
        let env_store = Arc::new(EnvStoreBuilder::new().explicit("FOO", "bar").build());

        let info = ProcessInfo::new(env_store, vec!["otter".into()]);
        let js = info.to_js_setup();

        assert!(js.contains("globalThis.process"));
        assert!(js.contains("envProxy"));
        assert!(js.contains("FOO"));
    }

    #[test]
    fn test_platform_values() {
        let platform = get_platform();
        assert!(["darwin", "linux", "win32", "unknown"].contains(&platform));

        let arch = get_arch();
        assert!(["x64", "arm64", "ia32", "arm", "unknown"].contains(&arch));
    }
}
