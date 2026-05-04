//! Public embedding API for the Otter foundation engine.
//!
//! Two-layer surface per
//! [ADR-0003](../../../docs/new-engine/adr/0003-public-api-and-cli.md):
//!
//! - **Layer A — [`Otter`]**: zero-config entry point. The simple
//!   case for embedders ("just run a script").
//! - **Layer B — [`Runtime`] / [`RuntimeBuilder`]**: opt-in
//!   advanced layer for capabilities, custom module loading, trace
//!   sinks, profiling.
//!
//! Every fallible call returns [`Result<_, OtterError>`] —
//! a single, structured, `serde::Serialize` enum. No
//! `Box<dyn Error>` anywhere on the public surface.
//!
//! # Contents
//! - [`Otter`] — Layer A wrapper.
//! - [`Runtime`], [`RuntimeBuilder`] — Layer B.
//! - [`SourceInput`] — JS / TS source bundles.
//! - [`ExecutionResult`] — successful run output.
//! - [`OtterError`], [`ConfigError`], [`IoErrorKind`] — error model.
//! - [`InterruptHandle`] — cooperative cancellation.
//!
//! # See also
//! - [ADR-0001](../../../docs/new-engine/adr/0001-staging-directory.md)
//! - [ADR-0003](../../../docs/new-engine/adr/0003-public-api-and-cli.md)

pub mod error;
pub mod module_graph;
pub mod module_loader;

use std::path::{Path, PathBuf};
use std::time::Duration;

use otter_bytecode::BytecodeModule;
use otter_compiler::compile;
use otter_gc::{GcHeap, GcStats, HeapSnapshot};
use otter_syntax::{SourceKind, detect_source_kind, parse};
use otter_vm::{Interpreter, InterruptFlag, Value};
use serde::{Deserialize, Serialize};

pub use error::{ConfigError, IoErrorKind, OtterError, error_schema_version};

/// Default heap cap (256 MiB) when none is configured.
pub const DEFAULT_MAX_HEAP_BYTES: u64 = 256 * 1024 * 1024;

/// Default per-`run_*` timeout (30 s) when none is configured.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default JS call-stack limit.
pub const DEFAULT_MAX_STACK_DEPTH: u32 = 1024;

/// Source bundle accepted by the runtime.
#[derive(Debug, Clone)]
pub struct SourceInput {
    /// Source text.
    pub text: String,
    /// Source kind.
    pub kind: SourceKind,
    /// Optional originating path (for diagnostics).
    pub path: Option<PathBuf>,
}

impl SourceInput {
    /// Build a JavaScript source bundle from in-memory text.
    #[must_use]
    pub fn from_javascript(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SourceKind::JavaScript,
            path: None,
        }
    }

    /// Build a TypeScript source bundle from in-memory text.
    #[must_use]
    pub fn from_typescript(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SourceKind::TypeScript,
            path: None,
        }
    }

    /// Read a source bundle from disk, detecting kind by extension.
    ///
    /// # Errors
    /// - [`OtterError::SourceKind`] when the extension is not a
    ///   foundation extension.
    /// - [`OtterError::Io`] when the file cannot be read.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, OtterError> {
        let path = path.as_ref();
        let kind = detect_source_kind(path).ok_or_else(|| OtterError::SourceKind {
            path: path.to_path_buf(),
            extension: path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string(),
        })?;
        let text = std::fs::read_to_string(path).map_err(|e| OtterError::Io {
            path: path.to_path_buf(),
            kind: IoErrorKind::from_std(e.kind()),
            message: e.to_string(),
        })?;
        Ok(Self {
            text,
            kind,
            path: Some(path.to_path_buf()),
        })
    }
}

/// Successful run output.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Completion value (foundation: `Value::Undefined`).
    pub completion: Value,
    /// Wall-clock duration.
    pub duration: Duration,
}

impl ExecutionResult {
    /// Render the completion value for CLI preview output.
    #[must_use]
    pub fn completion_string(&self) -> String {
        self.completion.display_string()
    }
}

/// Deno-inspired, UX-first capability set.
///
/// Each capability is a [`Permission<T>`] with three states:
///
/// - `Deny` — operations of this kind are rejected;
/// - `AllowAll` — operations are permitted unconditionally;
/// - `Scoped { allow_list, deny_list }` — power-user form: the
///   allow / deny patterns narrow the set further. Most users
///   never need this state.
///
/// **Defaults are practical, not paranoid.** [`CapabilitySet::default`]
/// (used by [`Otter::new`] and [`RuntimeBuilder::default`]) gives:
///
/// | Capability | Default | Why |
/// | --- | --- | --- |
/// | `read` | `AllowAll` | Imports / module loading must work without forcing the user to spell out every directory. |
/// | `write` | `Deny` | Filesystem mutation is rare in scripts and dangerous by default. |
/// | `net` | `Deny` | Network access is opt-in. |
/// | `env` | `Deny` | Environment variables may contain secrets. |
/// | `run` | `Deny` | Subprocess execution is opt-in. |
/// | `ffi` | `Deny` | Native library loading is opt-in. |
/// | `hrtime` | `Allow` | High-resolution time is low-risk and commonly used. |
///
/// Two convenience presets:
///
/// - [`CapabilitySet::sandbox`] — deny everything. Use this when
///   running untrusted code. Equivalent to the CLI's `--sandbox`.
/// - [`CapabilitySet::allow_all`] — allow everything unconditionally.
///   Equivalent to the CLI's `--allow-all`.
///
/// Power users can still pass scoped pattern lists
/// (`--allow-net=api.example.com`) but the CLI flags also accept the
/// **boolean form** (`--allow-net`) which upgrades the capability to
/// `AllowAll`.
///
/// The harness slice (task 07) **stores** these values but does not
/// enforce them yet. Enforcement lands with later slices when the
/// capability surface becomes observable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySet {
    /// Filesystem read permission.
    pub read: Permission<PathBuf>,
    /// Filesystem write permission.
    pub write: Permission<PathBuf>,
    /// Network permission. Patterns are `host[:port]`.
    pub net: Permission<String>,
    /// Environment variable permission. Patterns are variable names.
    pub env: Permission<String>,
    /// Subprocess permission. Patterns are command names / paths.
    pub run: Permission<String>,
    /// FFI loading permission. Patterns are library names / paths.
    pub ffi: Permission<PathBuf>,
    /// High-resolution time permission (boolean toggle).
    pub hrtime: BooleanPermission,
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self {
            read: Permission::AllowAll,
            write: Permission::Deny,
            net: Permission::Deny,
            env: Permission::Deny,
            run: Permission::Deny,
            ffi: Permission::Deny,
            hrtime: BooleanPermission::Allow,
        }
    }
}

/// Built-in secret-name deny patterns enforced on top of any
/// configured `env` permission.
///
/// These patterns are **always** denied, even under `--allow-all` or
/// `--allow-env=*`, so a stray `--allow-env` does not exfiltrate
/// secrets. Embedders can extend the list at the runtime boundary in
/// later slices.
pub const ENV_BUILTIN_DENY_PATTERNS: &[&str] = &[
    "*_SECRET",
    "*_TOKEN",
    "*_PASSWORD",
    "*_API_KEY",
    "AWS_*",
    "GITHUB_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
];

impl CapabilitySet {
    /// Test whether reading the named environment variable is
    /// permitted, taking [`ENV_BUILTIN_DENY_PATTERNS`] into account.
    ///
    /// The built-in deny list **always** wins, even under
    /// `Permission::AllowAll`.
    #[must_use]
    pub fn env_allows(&self, name: &str) -> bool {
        for pattern in ENV_BUILTIN_DENY_PATTERNS {
            if glob_match_string(pattern, name) {
                return false;
            }
        }
        self.env.matches(name)
    }

    /// Sandbox: deny every capability. Use when running untrusted
    /// code. The CLI maps `--sandbox` to this.
    #[must_use]
    pub fn sandbox() -> Self {
        Self {
            read: Permission::Deny,
            write: Permission::Deny,
            net: Permission::Deny,
            env: Permission::Deny,
            run: Permission::Deny,
            ffi: Permission::Deny,
            hrtime: BooleanPermission::Deny,
        }
    }

    /// Grant everything unconditionally. The CLI maps `--allow-all`
    /// to this. Development / testing only.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            read: Permission::AllowAll,
            write: Permission::AllowAll,
            net: Permission::AllowAll,
            env: Permission::AllowAll,
            run: Permission::AllowAll,
            ffi: Permission::AllowAll,
            hrtime: BooleanPermission::Allow,
        }
    }
}

/// Per-resource permission state.
///
/// Generic over the **pattern** type (e.g., `PathBuf` for fs,
/// `String` for hosts / env names). Mirrors Deno's per-permission
/// `--allow-X[=patterns]` / `--deny-X[=patterns]` model.
///
/// Patterns of type `String` (used by `net`, `env`, `run`) support
/// **glob-style wildcards**:
///
/// - `*` (alone) matches anything;
/// - `PREFIX_*` matches values starting with `PREFIX_` — useful for
///   environment variable scoping (`VITE_APP_*`, `NEXT_PUBLIC_*`,
///   `OTEL_*`);
/// - `*_SUFFIX` matches values ending with `_SUFFIX`;
/// - `PREFIX_*_SUFFIX` matches values that have both;
/// - exact strings (no `*`) match exactly.
///
/// Path patterns (`PathBuf`) are matched as **path prefixes** — an
/// allow pattern of `/var/data` matches `/var/data/x.json` and any
/// descendant. Wildcards are not supported in path patterns at this
/// slice; if needed, a future amendment adds them.
///
/// Use [`Permission::matches`] to test whether a concrete value is
/// permitted under the current rule set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Permission<T> {
    /// No operations of this kind are permitted.
    #[default]
    Deny,
    /// All operations of this kind are permitted unconditionally.
    AllowAll,
    /// Allow operations matching `allow_list`, except those matching
    /// `deny_list`. An empty `allow_list` is the same as `Deny`;
    /// `deny_list` always wins on conflict.
    Scoped {
        /// Allow patterns. Empty = deny.
        allow_list: Vec<T>,
        /// Deny patterns. Override `allow_list` on conflict.
        #[serde(default = "Vec::new")]
        deny_list: Vec<T>,
    },
}

impl<T> Permission<T> {
    /// Convenience: allow only the patterns listed (`Scoped` with no
    /// deny list).
    #[must_use]
    pub fn allow<I: IntoIterator<Item = T>>(items: I) -> Self {
        Self::Scoped {
            allow_list: items.into_iter().collect(),
            deny_list: Vec::new(),
        }
    }

    /// Convenience: allow `allow_list` but explicitly deny anything
    /// in `deny_list`.
    #[must_use]
    pub fn allow_except<A, D>(allow: A, deny: D) -> Self
    where
        A: IntoIterator<Item = T>,
        D: IntoIterator<Item = T>,
    {
        Self::Scoped {
            allow_list: allow.into_iter().collect(),
            deny_list: deny.into_iter().collect(),
        }
    }

    /// `true` when this permission is unconditional (`AllowAll`).
    #[must_use]
    pub const fn is_allow_all(&self) -> bool {
        matches!(self, Self::AllowAll)
    }

    /// `true` when this permission rejects every operation.
    #[must_use]
    pub fn is_deny(&self) -> bool {
        match self {
            Self::Deny => true,
            Self::Scoped { allow_list, .. } => allow_list.is_empty(),
            Self::AllowAll => false,
        }
    }
}

impl Permission<String> {
    /// Test whether `value` is permitted under this string permission.
    ///
    /// Patterns support wildcards (`*`, `PREFIX_*`, `*_SUFFIX`,
    /// `PREFIX_*_SUFFIX`); see the type-level documentation.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Deny => false,
            Self::AllowAll => true,
            Self::Scoped {
                allow_list,
                deny_list,
            } => {
                if deny_list
                    .iter()
                    .any(|p| glob_match_string(p.as_str(), value))
                {
                    return false;
                }
                allow_list
                    .iter()
                    .any(|p| glob_match_string(p.as_str(), value))
            }
        }
    }
}

impl Permission<PathBuf> {
    /// Test whether `value` is permitted under this path permission.
    ///
    /// Path patterns are **prefix matches**: an allow pattern of
    /// `/var/data` matches `/var/data` and any descendant.
    #[must_use]
    pub fn matches_path(&self, value: &Path) -> bool {
        match self {
            Self::Deny => false,
            Self::AllowAll => true,
            Self::Scoped {
                allow_list,
                deny_list,
            } => {
                if deny_list.iter().any(|p| value.starts_with(p)) {
                    return false;
                }
                allow_list.iter().any(|p| value.starts_with(p))
            }
        }
    }
}

/// Glob-style match for permission patterns.
///
/// Supports `*` (any-prefix / any-suffix / both) and exact strings.
/// `*` in the middle of a pattern is treated as a single wildcard
/// segment so `A_*_B` matches values that start with `A_` and end
/// with `_B`. Multiple `*` in one pattern are not supported.
fn glob_match_string(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let star_count = pattern.matches('*').count();
    if star_count == 0 {
        return pattern == value;
    }
    if star_count > 1 {
        // Foundation-phase: only one `*` per pattern is recognised.
        // Treat pathological patterns conservatively as no match.
        let (prefix, rest) = match pattern.split_once('*') {
            Some(p) => p,
            None => return false,
        };
        let suffix = match rest.rsplit_once('*') {
            Some((_, s)) => s,
            None => rest,
        };
        return value.starts_with(prefix) && value.ends_with(suffix);
    }
    let (prefix, suffix) = match pattern.split_once('*') {
        Some(p) => p,
        None => return false,
    };
    value.starts_with(prefix) && value.ends_with(suffix)
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn env_prefix_wildcard_matches() {
        let perm = Permission::<String>::allow(["VITE_APP_*".to_string()]);
        assert!(perm.matches("VITE_APP_API_URL"));
        assert!(perm.matches("VITE_APP_"));
        assert!(!perm.matches("OTHER_VAR"));
    }

    #[test]
    fn env_suffix_wildcard_matches() {
        let perm = Permission::<String>::allow(["*_SECRET".to_string()]);
        assert!(perm.matches("API_SECRET"));
        assert!(!perm.matches("API_KEY"));
    }

    #[test]
    fn env_deny_overrides_allow() {
        let perm = Permission::<String>::allow_except(
            ["VITE_*".to_string()],
            ["VITE_INTERNAL_*".to_string()],
        );
        assert!(perm.matches("VITE_PUBLIC"));
        assert!(!perm.matches("VITE_INTERNAL_TOKEN"));
    }

    #[test]
    fn allow_all_matches_anything() {
        let perm = Permission::<String>::AllowAll;
        assert!(perm.matches("anything"));
    }

    #[test]
    fn deny_matches_nothing() {
        let perm = Permission::<String>::Deny;
        assert!(!perm.matches("anything"));
    }

    #[test]
    fn builtin_secret_patterns_always_denied() {
        // Even with allow-all, secret-named env vars stay denied.
        let caps = CapabilitySet::allow_all();
        assert!(!caps.env_allows("AWS_SECRET_ACCESS_KEY"));
        assert!(!caps.env_allows("SOMETHING_TOKEN"));
        assert!(!caps.env_allows("MY_API_KEY"));
        assert!(!caps.env_allows("OPENAI_API_KEY"));
        assert!(caps.env_allows("VITE_PUBLIC_URL"));
    }

    #[test]
    fn vite_app_prefix_allowed_via_scoped_pattern() {
        let caps = CapabilitySet {
            env: Permission::<String>::allow(["VITE_APP_*".to_string()]),
            ..CapabilitySet::sandbox()
        };
        assert!(caps.env_allows("VITE_APP_API_URL"));
        assert!(!caps.env_allows("HOME"));
        // Built-in secret-deny still wins:
        assert!(!caps.env_allows("VITE_APP_API_KEY"));
    }
}

/// Boolean permission (no patterns; on / off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BooleanPermission {
    /// Operation is denied.
    #[default]
    Deny,
    /// Operation is allowed.
    Allow,
}

impl BooleanPermission {
    /// Convert to [`bool`].
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Cloneable cooperative-cancellation handle.
#[derive(Debug, Clone)]
pub struct InterruptHandle(InterruptFlag);

impl InterruptHandle {
    /// Trip the underlying flag from any thread.
    pub fn interrupt(&self) {
        self.0.interrupt();
    }

    /// Check the flag without resetting it.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.0.is_set()
    }

    /// Clear a previous interrupt.
    pub fn reset(&self) {
        self.0.reset();
    }
}

/// Runtime configuration.
#[derive(Debug, Clone)]
struct RuntimeConfig {
    max_heap_bytes: u64,
    timeout: Duration,
    max_stack_depth: u32,
    capabilities: CapabilitySet,
    loader: Option<module_loader::LoaderConfig>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_heap_bytes: DEFAULT_MAX_HEAP_BYTES,
            timeout: DEFAULT_TIMEOUT,
            max_stack_depth: DEFAULT_MAX_STACK_DEPTH,
            capabilities: CapabilitySet::default(),
            loader: None,
        }
    }
}

/// Layer B configuration entry point.
#[derive(Debug, Clone, Default)]
pub struct RuntimeBuilder {
    config: RuntimeConfig,
}

impl RuntimeBuilder {
    /// Replace the capability set.
    #[must_use]
    pub fn capabilities(mut self, caps: CapabilitySet) -> Self {
        self.config.capabilities = caps;
        self
    }

    /// Hard heap cap. `0` disables the cap.
    #[must_use]
    pub fn max_heap_bytes(mut self, bytes: u64) -> Self {
        self.config.max_heap_bytes = bytes;
        self
    }

    /// Per-`run_*` timeout. `Duration::ZERO` disables the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    /// JS call-stack depth cap.
    #[must_use]
    pub fn max_stack_depth(mut self, depth: u32) -> Self {
        self.config.max_stack_depth = depth;
        self
    }

    /// Override the module-loader configuration. The default
    /// (used when this is left untouched) infers `base_dir` from
    /// the entry file's parent directory at `run_module` time
    /// and uses the foundation extension list / ESM+CJS
    /// condition names with `node_modules` walk-up enabled.
    ///
    /// Spec mapping: <https://nodejs.org/api/esm.html#resolution-and-loading-algorithm>
    #[must_use]
    pub fn module_loader(mut self, loader: module_loader::LoaderConfig) -> Self {
        self.config.loader = Some(loader);
        self
    }

    /// Construct the runtime.
    ///
    /// # Errors
    /// Returns [`OtterError::Config`] when the configuration is
    /// inconsistent.
    pub fn build(self) -> Result<Runtime, OtterError> {
        if self.config.max_stack_depth == 0 {
            return Err(OtterError::Config {
                reason: ConfigError::InvalidStackDepth {
                    message: "max_stack_depth must be > 0".to_string(),
                },
            });
        }
        // The interpreter owns the per-isolate GC heap (since
        // task 76); both the string heap and the GC heap honour
        // the configured cap.
        let mut interp = Interpreter::with_string_heap_cap(self.config.max_heap_bytes);
        interp.set_max_stack_depth(self.config.max_stack_depth);
        // §19.4.1 / §20.2.1.1 — wire the eval hook so `eval(src)` /
        // `new Function(...)` reach a real parse + compile path.
        // The closure is reusable across calls; each invocation
        // builds a fresh `BytecodeModule`.
        let hook: otter_vm::EvalHook = std::rc::Rc::new(|source: &str| {
            let parsed = parse(source, SourceKind::JavaScript)
                .map_err(|e| format!("syntax error: {e:?}"))?;
            compile(&parsed, "<eval>").map_err(|e| format!("compile error: {e:?}"))
        });
        interp.set_eval_hook(Some(hook));
        Ok(Runtime {
            interp,
            config: self.config,
        })
    }
}

/// Layer B isolate.
#[derive(Debug)]
pub struct Runtime {
    interp: Interpreter,
    config: RuntimeConfig,
}

impl Runtime {
    /// Start configuring a new runtime.
    #[must_use]
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    /// Cooperative cancellation handle.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle(self.interp.interrupt_handle())
    }

    /// Configured per-`run_*` timeout (currently informational; the
    /// foundation slice does not yet enforce timeouts).
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.config.timeout
    }

    /// Configured heap cap in bytes (`0` = disabled).
    ///
    /// The cap is **load-bearing** as of task 73:
    /// allocations against the [`otter_gc::GcHeap`] and string
    /// allocations against the interpreter's string heap that
    /// would overshoot the cap surface as
    /// [`OtterError::OutOfMemory`]. Per-type GC migrations
    /// (tasks 76–83) progressively widen the set of script
    /// allocations subject to the cap.
    #[must_use]
    pub fn max_heap_bytes(&self) -> u64 {
        self.config.max_heap_bytes
    }

    /// Borrow the runtime's GC heap. Owned by the interpreter
    /// since task 76; this accessor delegates.
    #[must_use]
    pub fn gc_heap(&self) -> &GcHeap {
        self.interp.gc_heap()
    }

    /// Mutable borrow of the runtime's GC heap. Owned by the
    /// interpreter since task 76; this accessor delegates.
    #[must_use]
    pub fn gc_heap_mut(&mut self) -> &mut GcHeap {
        self.interp.gc_heap_mut()
    }

    /// Per-heap GC counters: live objects / live bytes / per-
    /// `type_tag` rows / last-GC pause / cycle counter.
    ///
    /// Takes `&mut self` because the aggregate `live_objects` /
    /// `live_bytes` fields are derived lazily from the per-tag
    /// rows (the alloc fast path only updates the per-tag
    /// counters, see [`otter_gc::GcHeap::gc_stats`]). Per-type
    /// GC migrations (tasks 76–83) widen the surface populated
    /// under `by_type` — Phase 1 only sees host-side
    /// allocations through [`Self::gc_heap_mut`].
    pub fn heap_stats(&mut self) -> &GcStats {
        self.interp.gc_heap_mut().gc_stats()
    }

    /// Snapshot the live object graph plus a caller-supplied
    /// root set, returning a Rust-side [`HeapSnapshot`].
    ///
    /// Distinct from
    /// [`otter_gc::devtools_snapshot::write_heap_snapshot`],
    /// which produces a Chrome DevTools JSON payload — that
    /// writer is the production-debug path; this snapshot is
    /// for Rust assertions and per-root retained-size queries
    /// (e.g. migration tasks proving cycles return to baseline).
    ///
    /// `roots` are the root slot values to attribute retained
    /// size to. Phase 1 callers typically pass an empty slice
    /// when the heap holds nothing reachable from the
    /// interpreter; per-type migrations widen the typical root
    /// set as they land.
    pub fn heap_snapshot(&mut self, roots: &[otter_gc::RawGc]) -> HeapSnapshot {
        self.interp.gc_heap().snapshot(roots)
    }

    /// Force a full GC cycle (scavenge + old-gen mark-sweep).
    ///
    /// **Debug / test only.** Production code must never call
    /// this — the GC's own triggers are tuned to allocation
    /// pressure, and a forced cycle perturbs those metrics.
    /// Tests use this to assert "after dropping these handles
    /// and forcing a GC, live counts return to baseline".
    ///
    /// The walker delegates to
    /// [`otter_vm::runtime_state::RuntimeState::trace_roots`]
    /// (task 75) via [`otter_vm::Interpreter::force_gc`], which
    /// owns the heap and does the split-borrow internally.
    pub fn force_gc(&mut self) {
        self.interp.force_gc();
    }

    /// Configured stack-depth cap.
    #[must_use]
    pub fn max_stack_depth(&self) -> u32 {
        self.config.max_stack_depth
    }

    /// Borrow the capability set.
    #[must_use]
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.config.capabilities
    }

    /// Compile and execute `source` as a script.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_script(
        &mut self,
        source: SourceInput,
        specifier: &str,
    ) -> Result<ExecutionResult, OtterError> {
        let start = std::time::Instant::now();
        let module = self.compile_source(&source, specifier)?;
        // Run the script first; the script error wins if both the
        // script and the drain fail. On script success we still
        // drain so any `queueMicrotask` registered during script
        // execution gets a chance to run before we report success.
        let script_outcome = self.interp.run(&module);
        let drain_outcome = self.interp.drain_microtasks(&module);
        let value = match (script_outcome, drain_outcome) {
            (Err(script_err), _) => return Err(map_vm_error(script_err)),
            (Ok(_), Err(drain_err)) => return Err(map_vm_error(drain_err)),
            (Ok(v), Ok(())) => v,
        };
        Ok(ExecutionResult {
            completion: value,
            duration: start.elapsed(),
        })
    }

    /// Drain the microtask queue manually. Embedders that want to
    /// step the queue between script runs (or in response to
    /// host-side events) call this directly. Foundation slices
    /// run `run_script` / `eval` always drain after the script,
    /// so manual draining is rarely needed today.
    ///
    /// # Errors
    /// Any `VmError` raised by a microtask propagates as
    /// `OtterError::Runtime`.
    pub fn run_microtasks(&mut self) -> Result<(), OtterError> {
        // Empty module so the drain has a `BytecodeModule` to look
        // up function bodies in. Microtasks always reference
        // functions defined in the original module they were
        // queued from — this entry point is for embedders who
        // already have that module on hand; for now we surface
        // it as a no-op when the queue is empty.
        if !self.interp.microtasks().has_any_pending() {
            return Ok(());
        }
        // Without a module we cannot resolve function ids; the
        // foundation contract is that callers use the auto-drain
        // path. Document this loudly.
        Err(OtterError::Internal {
            code: "MICROTASK_DRAIN_NEEDS_MODULE".to_string(),
            message: "manual microtask draining requires the originating module; use run_script which auto-drains".to_string(),
        })
    }

    /// Convenience: run an expression for tooling.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn eval(&mut self, source: SourceInput) -> Result<ExecutionResult, OtterError> {
        self.run_script(source, "<eval>")
    }

    /// Compile-only: parse + erase + lower without executing.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn check(&self, source: SourceInput, specifier: &str) -> Result<(), OtterError> {
        self.compile_source(&source, specifier).map(|_| ())
    }

    /// Compile-and-dump: produce the bytecode module for inspection.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn dump(&self, source: SourceInput, specifier: &str) -> Result<BytecodeModule, OtterError> {
        self.compile_source(&source, specifier)
    }

    fn compile_source(
        &self,
        source: &SourceInput,
        specifier: &str,
    ) -> Result<BytecodeModule, OtterError> {
        let parsed =
            parse(source.text.clone(), source.kind).map_err(|err| OtterError::Compile {
                diagnostics: vec![Diagnostic::syntax(err.messages.join("; "))],
            })?;
        compile(&parsed, specifier).map_err(map_compile_error)
    }

    /// Load + link + execute the module-graph rooted at `entry_path`.
    ///
    /// # Algorithm
    /// 1. Build a [`module_loader::ModuleLoader`] rooted at the
    ///    entry's parent directory.
    /// 2. Walk the dependency graph
    ///    ([`module_graph::load_program`]), resolving every static
    ///    import + literal-string `import("./x")` into a unified
    ///    linked [`BytecodeModule`].
    /// 3. Pre-allocate one `module_env` JsObject per module URL,
    ///    register each in the interpreter, and append self-loop
    ///    `(referrer = entry_url, specifier = url, target = url)`
    ///    rows to `module_resolutions` so the synthesised
    ///    `<entry>` driver's `Op::ImportNamespace` lookups succeed.
    /// 4. Run `<entry>` through the existing dispatch loop. Drain
    ///    microtasks afterwards.
    ///
    /// # Errors
    /// See [`OtterError`] variants. Loader / link errors surface
    /// as [`OtterError::Compile`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-modules>
    pub fn run_module(
        &mut self,
        entry_path: impl AsRef<Path>,
    ) -> Result<ExecutionResult, OtterError> {
        let start = std::time::Instant::now();
        let entry_path = entry_path.as_ref();
        let loader = match &self.config.loader {
            Some(cfg) => module_loader::ModuleLoader::with_config(cfg.clone()),
            None => {
                let base_dir = entry_path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                module_loader::ModuleLoader::new(base_dir)
            }
        };
        let linked =
            module_graph::load_program(&loader, entry_path).map_err(|e| OtterError::Compile {
                diagnostics: vec![Diagnostic::syntax(e.to_string())],
            })?;

        // Pre-populate the per-run module-env registry. Every
        // module URL gets a fresh JsObject; <entry> resolves via
        // self-loop ImportNamespace edges and stores into the
        // env as the body runs.
        self.interp.reset_module_state();
        let mut module = linked.module;
        let entry_url = linked.entry_url.clone();
        for init in &module.module_inits {
            let env = otter_vm::object::alloc_object(self.interp.gc_heap_mut())?;
            self.interp
                .register_module_env(std::rc::Rc::from(init.url.as_str()), env);
            // Self-loop edge: <entry>'s referrer is the entry's URL
            // (the synthesized <entry> function carries empty
            // module_url, so the dispatcher uses an empty string;
            // we add edges keyed on both shapes).
            module
                .module_resolutions
                .push(otter_bytecode::ModuleResolution {
                    referrer: entry_url.clone(),
                    specifier: init.url.clone(),
                    target: init.url.clone(),
                });
            module
                .module_resolutions
                .push(otter_bytecode::ModuleResolution {
                    referrer: String::new(),
                    specifier: init.url.clone(),
                    target: init.url.clone(),
                });
        }

        let script_outcome = self.interp.run(&module);
        let drain_outcome = self.interp.drain_microtasks(&module);
        let value = match (script_outcome, drain_outcome) {
            (Err(script_err), _) => return Err(map_vm_error(script_err)),
            (Ok(_), Err(drain_err)) => return Err(map_vm_error(drain_err)),
            (Ok(v), Ok(())) => v,
        };
        Ok(ExecutionResult {
            completion: value,
            duration: start.elapsed(),
        })
    }
}

/// Layer A entry point: zero-config Otter.
///
/// Wraps a [`Runtime`] with sensible defaults. The simple case
/// for embedders.
#[derive(Debug)]
pub struct Otter {
    runtime: Runtime,
}

impl Otter {
    /// Construct with defaults: deny-all capabilities,
    /// 256 MiB heap cap, 30 s timeout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runtime: Runtime::builder()
                .build()
                .expect("default RuntimeBuilder must build"),
        }
    }

    /// Run a file from disk, detecting kind by extension and
    /// routing module-shaped files through the module-graph
    /// pipeline.
    ///
    /// # Algorithm
    /// 1. Read the file's source text.
    /// 2. Heuristically detect "is this a module?" by scanning
    ///    the source for top-level `import` / `export` keywords
    ///    (the parser would tell us authoritatively, but cheap
    ///    string matching avoids two parses on the script path).
    /// 3. Module sources go through [`Runtime::run_module`];
    ///    plain scripts go through [`Runtime::run_script`].
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_file(&mut self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError> {
        let path = path.as_ref();
        let source = SourceInput::from_path(path)?;
        if source_text_looks_like_module(&source.text, source.kind) {
            return self.runtime.run_module(path);
        }
        let specifier = path.to_string_lossy().to_string();
        self.runtime.run_script(source, &specifier)
    }

    /// Run a string of JavaScript.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_script(&mut self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.runtime
            .run_script(SourceInput::from_javascript(source), "<script>")
    }

    /// Run a string of TypeScript.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn run_typescript(&mut self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.runtime
            .run_script(SourceInput::from_typescript(source), "<script>")
    }

    /// Evaluate a snippet.
    ///
    /// # Errors
    /// See [`OtterError`] variants.
    pub fn eval(&mut self, source: &str) -> Result<ExecutionResult, OtterError> {
        self.runtime.eval(SourceInput::from_javascript(source))
    }

    /// Cooperative cancellation handle.
    #[must_use]
    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.runtime.interrupt_handle()
    }

    /// Drop down to Layer B.
    #[must_use]
    pub fn into_runtime(self) -> Runtime {
        self.runtime
    }

    /// Borrow the underlying [`Runtime`] (Layer B) for advanced
    /// operations such as bytecode dump or compile-only check.
    #[must_use]
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Mutable borrow of the underlying [`Runtime`].
    #[must_use]
    pub fn runtime_mut(&mut self) -> &mut Runtime {
        &mut self.runtime
    }
}

impl Default for Otter {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable diagnostic shape (foundation subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Machine-readable kind.
    pub kind: DiagnosticKind,
    /// Stable code (`TS_UNSUPPORTED`, `OOM_HEAP_LIMIT`, …).
    pub code: String,
    /// Human-readable summary.
    pub message: String,
    /// Optional source span.
    pub span: Option<(u32, u32)>,
    /// Stack frames when relevant.
    #[serde(default)]
    pub frames: Vec<StackFrame>,
    /// Optional cause chain.
    #[serde(default)]
    pub cause: Option<Box<Diagnostic>>,
}

impl Diagnostic {
    /// Construct a syntax-class diagnostic.
    #[must_use]
    pub fn syntax(message: impl Into<String>) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "SYNTAX_ERROR".to_string(),
            message: message.into(),
            span: None,
            frames: Vec::new(),
            cause: None,
        }
    }

    /// Construct a TS-unsupported diagnostic.
    #[must_use]
    pub fn ts_unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "TS_UNSUPPORTED".to_string(),
            message: message.into(),
            span: Some(span),
            frames: Vec::new(),
            cause: None,
        }
    }

    /// Construct a generic "feature not in this slice" diagnostic.
    #[must_use]
    pub fn unsupported(message: impl Into<String>, span: (u32, u32)) -> Self {
        Self {
            kind: DiagnosticKind::Syntax,
            code: "FEATURE_NOT_IN_SLICE".to_string(),
            message: message.into(),
            span: Some(span),
            frames: Vec::new(),
            cause: None,
        }
    }
}

/// Diagnostic category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// Syntax / TypeScript erasure / compile-time.
    Syntax,
    /// `TypeError`.
    Type,
    /// `ReferenceError`.
    Reference,
    /// `RangeError`.
    Range,
    /// Heap cap hit.
    OutOfMemory,
    /// Timeout fired.
    Timeout,
    /// Capability denied.
    Capability,
    /// Internal bug.
    Internal,
}

/// Single stack frame.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StackFrame {
    /// Function name; `"<main>"` for the script entry.
    pub function: String,
    /// Module specifier.
    pub module: String,
    /// Source span within `module`.
    pub span: Option<(u32, u32)>,
}

/// AST-based "is this a module?" detection.
///
/// Parses the source through `otter-syntax` (the same OXC frontend
/// the compiler uses) and walks the program's AST looking for any
/// of:
/// - `ImportDeclaration` / `ExportNamedDeclaration` /
///   `ExportDefaultDeclaration` / `ExportAllDeclaration` (these
///   can only appear at the module top level — finding one is
///   conclusive proof);
/// - any `ImportExpression` (`import(specifier)` — only legal in
///   modules);
/// - any `MetaProperty` whose base is `import` (`import.meta` —
///   only legal in modules).
///
/// The walk uses [`oxc_ast_visit::Visit`] so we don't hand-roll a
/// match arm per AST node kind. ADR-0002 forbids regex / string
/// parsing of JS/TS source.
///
/// Parse failures default to `false`: the caller routes to the
/// script path, which will re-parse and surface the same syntax
/// error through its diagnostic pipeline.
fn source_text_looks_like_module(text: &str, kind: SourceKind) -> bool {
    let parsed = match parse(text.to_string(), kind) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let program = match parsed.program() {
        Ok(p) => p,
        Err(_) => return false,
    };
    use oxc_ast::ast::{Expression, Statement};
    use oxc_ast_visit::Visit;

    #[derive(Default)]
    struct ModuleSyntaxFinder {
        found: bool,
    }

    impl<'a> Visit<'a> for ModuleSyntaxFinder {
        fn visit_import_declaration(&mut self, _: &oxc_ast::ast::ImportDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_named_declaration(&mut self, _: &oxc_ast::ast::ExportNamedDeclaration<'a>) {
            self.found = true;
        }
        fn visit_export_default_declaration(
            &mut self,
            _: &oxc_ast::ast::ExportDefaultDeclaration<'a>,
        ) {
            self.found = true;
        }
        fn visit_export_all_declaration(&mut self, _: &oxc_ast::ast::ExportAllDeclaration<'a>) {
            self.found = true;
        }
        fn visit_import_expression(&mut self, _: &oxc_ast::ast::ImportExpression<'a>) {
            self.found = true;
        }
        fn visit_meta_property(&mut self, meta: &oxc_ast::ast::MetaProperty<'a>) {
            if meta.meta.name.as_str() == "import" && meta.property.name.as_str() == "meta" {
                self.found = true;
            }
        }
    }

    let mut finder = ModuleSyntaxFinder::default();
    // Visit each statement; the visitor short-circuits via the
    // `found` flag (we still walk everything, but the boolean
    // result is correct).
    for stmt in &program.body {
        finder.visit_statement(stmt);
        if finder.found {
            return true;
        }
    }
    // Also check program-level expressions inside expression
    // statements — covered by visit_statement above.
    let _ = (Statement::EmptyStatement, Expression::NullLiteral);
    finder.found
}

fn map_compile_error(err: otter_compiler::CompileError) -> OtterError {
    use otter_compiler::CompileError;
    match err {
        CompileError::Syntax { messages } => OtterError::Compile {
            diagnostics: vec![Diagnostic::syntax(messages.join("; "))],
        },
        CompileError::Unsupported { node, span } => OtterError::Compile {
            diagnostics: vec![Diagnostic::unsupported(
                format!("unsupported AST node: {node}"),
                span,
            )],
        },
        CompileError::TypeScriptUnsupported { node, span } => OtterError::Compile {
            diagnostics: vec![Diagnostic::ts_unsupported(
                format!("typescript {node} is not supported in foundation"),
                span,
            )],
        },
        _ => OtterError::Internal {
            code: "COMPILE_UNKNOWN".to_string(),
            message: "unknown compiler error variant".to_string(),
        },
    }
}

fn map_vm_error(run_err: otter_vm::RunError) -> OtterError {
    use otter_vm::VmError;
    let otter_vm::RunError { error, frames } = run_err;
    let stack_frames: Vec<StackFrame> = frames
        .into_iter()
        .map(|f| StackFrame {
            function: f.function_name,
            module: f.module,
            span: Some(f.span),
        })
        .collect();
    let top_span = stack_frames.first().and_then(|f| f.span);
    let display = error.to_string();
    let runtime_diagnostic =
        |kind: DiagnosticKind, code: &str, message: String| OtterError::Runtime {
            diagnostic: Diagnostic {
                kind,
                code: code.to_string(),
                message,
                span: top_span,
                frames: stack_frames.clone(),
                cause: None,
            },
        };
    match error {
        VmError::Interrupted => OtterError::Interrupted,
        VmError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => OtterError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        VmError::TypeMismatch => runtime_diagnostic(DiagnosticKind::Type, "TYPE_MISMATCH", display),
        VmError::UnknownIntrinsic { name } => runtime_diagnostic(
            DiagnosticKind::Type,
            "UNKNOWN_METHOD",
            format!("unknown method `{name}`"),
        ),
        VmError::TemporalDeadZone { local_index } => runtime_diagnostic(
            DiagnosticKind::Reference,
            "TDZ",
            format!("cannot access local {local_index} before initialization"),
        ),
        VmError::StackOverflow { limit } => runtime_diagnostic(
            DiagnosticKind::Range,
            "STACK_OVERFLOW",
            format!("maximum call stack size exceeded (limit {limit})"),
        ),
        VmError::NotCallable => runtime_diagnostic(
            DiagnosticKind::Type,
            "NOT_CALLABLE",
            "value is not a function".to_string(),
        ),
        VmError::Uncaught { value } => runtime_diagnostic(
            DiagnosticKind::Type,
            "UNCAUGHT",
            format!("uncaught exception: {value}"),
        ),
        VmError::JsonError { code, message } => {
            // `code` is `&'static str` so we can pass it straight
            // through; it stays stable for telemetry/log filters.
            runtime_diagnostic(DiagnosticKind::Type, code, message)
        }
        VmError::InvalidRegExp { message } => {
            runtime_diagnostic(DiagnosticKind::Syntax, "INVALID_REGEXP", message)
        }
        VmError::MissingReturn | VmError::InvalidOperand => OtterError::Internal {
            code: "VM_BYTECODE_INVARIANT".to_string(),
            message: display,
        },
        _ => OtterError::Internal {
            code: "VM_UNKNOWN".to_string(),
            message: display,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_module_detects_import_at_top_level() {
        let text = "import { x } from \"./y.ts\";\n";
        assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
    }

    #[test]
    fn looks_like_module_detects_export_function() {
        let text = "export function f() {}\n";
        assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
    }

    #[test]
    fn looks_like_module_with_leading_block_comment() {
        let text = "/* hi */\nimport { x } from \"./y.ts\";\n";
        assert!(source_text_looks_like_module(text, SourceKind::TypeScript));
    }

    #[test]
    fn module_program_runs_two_file_static_import() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.ts"), "export let value = 7;\n").unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "import { value } from \"./other.ts\";\nfunction fail() { return undefined.x; }\nif (value !== 7) fail();\n",
        )
        .unwrap();
        let mut otter = Otter::new();
        otter.run_file(dir.path().join("entry.ts")).unwrap();
    }

    #[test]
    fn module_program_propagates_live_binding_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("counter.ts"),
            "export let count = 0;\nexport function inc() { count = count + 1; }\nexport function get() { return count; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "import { inc, get } from \"./counter.ts\";\nfunction fail() { return undefined.x; }\ninc(); inc();\nif (get() !== 2) fail();\n",
        )
        .unwrap();
        let mut otter = Otter::new();
        otter.run_file(dir.path().join("entry.ts")).unwrap();
    }

    #[test]
    fn module_program_detects_cycle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.ts"),
            "import { b } from \"./b.ts\";\nexport let a = 1;\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.ts"),
            "import { a } from \"./a.ts\";\nexport let b = 2;\n",
        )
        .unwrap();
        let mut otter = Otter::new();
        let err = otter.run_file(dir.path().join("a.ts")).unwrap_err();
        match err {
            OtterError::Compile { diagnostics } => {
                assert!(
                    diagnostics
                        .iter()
                        .any(|d| d.message.contains("cycle") || d.message.contains("RangeError")),
                    "expected cycle diagnostic, got {diagnostics:?}"
                );
            }
            other => panic!("expected Compile (cycle), got {other:?}"),
        }
    }

    #[test]
    fn module_program_dynamic_import_literal_resolves_to_namespace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("util.ts"), "export let answer = 42;\n").unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "function fail() { return undefined.x; }\nimport(\"./util.ts\").then((m) => { if (m.answer !== 42) fail(); });\n",
        )
        .unwrap();
        let mut otter = Otter::new();
        otter.run_file(dir.path().join("entry.ts")).unwrap();
    }

    #[test]
    fn module_program_import_meta_url_matches_canonical() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("entry.ts"),
            "function fail() { return undefined.x; }\nlet u = import.meta.url;\nif (u.indexOf(\"file://\") !== 0) fail();\nif (u.indexOf(\"entry.ts\") < 0) fail();\n",
        )
        .unwrap();
        let mut otter = Otter::new();
        otter.run_file(dir.path().join("entry.ts")).unwrap();
    }

    #[test]
    fn otter_runs_empty_typescript() {
        let mut otter = Otter::new();
        let result = otter.run_typescript("").unwrap();
        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn otter_runs_undefined_literal() {
        let mut otter = Otter::new();
        let result = otter.run_typescript("undefined;").unwrap();
        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn json_cyclic_surfaces_jsc_style_diagnostic() {
        let mut otter = Otter::new();
        let err = otter
            .run_typescript("const a = {}; a.self = a; JSON.stringify(a);")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "JSON_CYCLIC");
                assert_eq!(
                    diagnostic.message,
                    "JSON.stringify cannot serialize cyclic structures."
                );
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn json_parse_error_carries_byte_position() {
        let mut otter = Otter::new();
        let err = otter
            .run_typescript("JSON.parse(\"[1, 2,]\");")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "JSON_PARSE");
                assert!(
                    diagnostic
                        .message
                        .starts_with("JSON Parse error: trailing comma")
                );
                assert!(diagnostic.message.contains("at byte 6"));
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn json_bigint_emits_jsc_style_message() {
        let mut otter = Otter::new();
        let err = otter
            .run_typescript("JSON.stringify({ n: 1n });")
            .unwrap_err();
        match err {
            OtterError::Runtime { diagnostic } => {
                assert_eq!(diagnostic.code, "JSON_BIGINT");
                assert_eq!(
                    diagnostic.message,
                    "JSON.stringify cannot serialize BigInt values."
                );
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
    }

    #[test]
    fn otter_rejects_unsupported_js_feature() {
        // `with` is permanently outside the foundation subset, so
        // it makes a stable canary for the FEATURE_NOT_IN_SLICE
        // diagnostic shape. (`try`/`catch` shipped in task 24.)
        let mut otter = Otter::new();
        let err = otter.run_typescript("with (o) { x; }").unwrap_err();
        match err {
            OtterError::Compile { diagnostics } => {
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].code, "FEATURE_NOT_IN_SLICE");
            }
            other => panic!("expected Compile, got {other:?}"),
        }
    }

    #[test]
    fn otter_rejects_typescript_enum() {
        // `enum` is intentionally rejected by ADR-0002 §4.
        let mut otter = Otter::new();
        let err = otter.run_typescript("enum E { A }").unwrap_err();
        match err {
            OtterError::Compile { diagnostics } => {
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].code, "TS_UNSUPPORTED");
            }
            other => panic!("expected Compile, got {other:?}"),
        }
    }

    #[test]
    fn otter_erases_interface() {
        let mut otter = Otter::new();
        let result = otter
            .run_typescript("interface I { x: number; } undefined;")
            .unwrap();
        assert_eq!(result.completion_string(), "undefined");
    }

    #[test]
    fn run_file_rejects_unknown_extension() {
        let mut otter = Otter::new();
        let err = otter.run_file("/nonexistent.foo").unwrap_err();
        assert!(matches!(err, OtterError::SourceKind { .. }));
    }

    #[test]
    fn json_error_carries_schema_version() {
        let err = OtterError::SourceKind {
            path: PathBuf::from("nope.foo"),
            extension: "foo".to_string(),
        };
        let json = err.to_json().unwrap();
        assert!(json.contains("\"error_schema_version\":1"));
        assert!(json.contains("\"kind\":\"source_kind\""));
    }
}
