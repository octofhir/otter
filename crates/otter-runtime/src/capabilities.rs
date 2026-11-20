//! Capability grants and path, environment and network matching.
//!
//! # Contents
//! Path and environment grants, matching rules and capability presets.
//!
//! # Invariants
//! Default grants deny access and checks precede host operations.
//!
//! # See also
//! - [`crate::Runtime`] owns the runtime session.

use crate::*;

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
/// [`CapabilitySet::default`] is deny-by-default. Embedders must opt in to
/// capabilities explicitly, or use [`CapabilitySet::allow_all`] for trusted
/// development scenarios.
///
/// | Capability | Default | Why |
/// | --- | --- | --- |
/// | `read` | `Deny` | Filesystem access is a host resource and must be explicit. |
/// | `write` | `Deny` | Filesystem mutation is rare in scripts and dangerous by default. |
/// | `net` | `Deny` | Network access is opt-in. |
/// | `env` | `Deny` | Environment variables may contain secrets. |
/// | `run` | `Deny` | Subprocess execution is opt-in. |
/// | `ffi` | `Deny` | Native library loading is opt-in. |
///
/// Two convenience presets:
///
/// - [`CapabilitySet::sandbox`] — deny everything. Equivalent to
///   [`CapabilitySet::default`] and the CLI's `--sandbox`.
/// - [`CapabilitySet::allow_all`] — allow everything unconditionally.
///   Equivalent to the CLI's `--allow-all`.
///
/// Power users can still pass scoped pattern lists
/// (`--allow-net=api.example.com`) but the CLI flags also accept the
/// **boolean form** (`--allow-net`) which upgrades the capability to
/// `AllowAll`.
///
/// Runtime and product crates must enforce these checks at the Rust boundary
/// before opening host resources or starting host work.
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
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self::sandbox()
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
        if env_name_is_builtin_denied(name) {
            return false;
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
        }
    }

    /// What this process hands to the ones it starts, or `None` when it hands
    /// them nothing.
    ///
    /// A process trusted with everything trusts what it starts with the same:
    /// it can already do anything its children could, and a program that
    /// launches a helper through a shell has no other way to say so. Anything
    /// short of everything is not passed on — a narrowed grant is about this
    /// program's own reach, and widening it for a child would be guessing.
    #[must_use]
    pub fn inherited_grant(&self) -> Option<&'static str> {
        self.is_allow_all().then_some(GRANT_EVERYTHING)
    }

    /// Whether every capability is granted.
    #[must_use]
    pub fn is_allow_all(&self) -> bool {
        self.read.is_allow_all()
            && self.write.is_allow_all()
            && self.net.is_allow_all()
            && self.env.is_allow_all()
            && self.run.is_allow_all()
            && self.ffi.is_allow_all()
    }

    /// What the process that started this one granted it, if anything.
    #[must_use]
    pub fn from_inherited_grant() -> Option<Self> {
        match std::env::var(GRANT_VAR).ok()?.as_str() {
            GRANT_EVERYTHING => Some(Self::allow_all()),
            _ => None,
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
        }
    }

    /// Narrow every capability by `requested`: the result permits an
    /// operation only when **both** this set and the requested set permit
    /// it, so a requested subset can never escalate past the grantor.
    /// Used for worker-scoped capability subsets.
    #[must_use]
    pub fn narrowed(&self, requested: Self) -> Self {
        Self {
            read: self.read.narrowed(requested.read),
            write: self.write.narrowed(requested.write),
            net: self.net.narrowed(requested.net),
            env: self.env.narrowed(requested.env),
            run: self.run.narrowed(requested.run),
            ffi: self.ffi.narrowed(requested.ffi),
        }
    }
}

/// Names what a launched process was granted by the one that started it. The
/// environment is the only thing that survives a shell standing between the
/// two, which is how a program most often launches another.
pub const GRANT_VAR: &str = "OTTER_GRANT";

/// The one grant that is passed on: everything.
const GRANT_EVERYTHING: &str = "all";

pub(super) fn env_name_is_builtin_denied(name: &str) -> bool {
    ENV_BUILTIN_DENY_PATTERNS
        .iter()
        .any(|pattern| glob_match_string(pattern, name))
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
    /// Both layers must allow; a deny in either wins. Produced by
    /// capability narrowing (worker-scoped subsets): pattern languages
    /// have no computable intersection, so the sound composition keeps
    /// both rule sets and evaluates them together at check time.
    Intersect {
        /// The granting layer being narrowed (e.g. the parent's rule set).
        outer: Box<Permission<T>>,
        /// The narrowing layer (e.g. the worker-requested subset).
        inner: Box<Permission<T>>,
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
    pub fn is_allow_all(&self) -> bool {
        match self {
            Self::AllowAll => true,
            Self::Intersect { outer, inner } => outer.is_allow_all() && inner.is_allow_all(),
            _ => false,
        }
    }

    /// `true` when this permission rejects every operation.
    #[must_use]
    pub fn is_deny(&self) -> bool {
        match self {
            Self::Deny => true,
            Self::Scoped { allow_list, .. } => allow_list.is_empty(),
            Self::AllowAll => false,
            Self::Intersect { outer, inner } => outer.is_deny() || inner.is_deny(),
        }
    }

    /// Narrow this permission by `requested`: the result allows an
    /// operation only when **both** rule sets allow it, so a requested
    /// subset can never escalate past `self`. Trivial layers collapse
    /// (`AllowAll` is the identity, `Deny` absorbs); the general case
    /// keeps both rule sets and evaluates them together.
    #[must_use]
    pub fn narrowed(&self, requested: Self) -> Self
    where
        T: Clone,
    {
        match (self, requested) {
            (Self::Deny, _) => Self::Deny,
            (_, Self::Deny) => Self::Deny,
            (Self::AllowAll, requested) => requested,
            (parent, Self::AllowAll) => parent.clone(),
            (parent, requested) => Self::Intersect {
                outer: Box::new(parent.clone()),
                inner: Box::new(requested),
            },
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
            Self::Intersect { outer, inner } => outer.matches(value) && inner.matches(value),
        }
    }

    /// Test alternate spellings of one resource as a single permission
    /// decision.
    ///
    /// Every deny pattern is checked against every spelling before any allow
    /// pattern is accepted. This is required for resources such as network
    /// endpoints, where `example.com` and `example.com:443` describe the same
    /// connection and evaluating them as independent `matches` calls would
    /// let one spelling bypass a deny on the other.
    #[must_use]
    pub fn matches_any(&self, values: &[&str]) -> bool {
        match self {
            Self::Deny => false,
            Self::AllowAll => !values.is_empty(),
            Self::Scoped {
                allow_list,
                deny_list,
            } => {
                if deny_list
                    .iter()
                    .any(|pattern| values.iter().any(|value| glob_match_string(pattern, value)))
                {
                    return false;
                }
                allow_list
                    .iter()
                    .any(|pattern| values.iter().any(|value| glob_match_string(pattern, value)))
            }
            Self::Intersect { outer, inner } => {
                outer.matches_any(values) && inner.matches_any(values)
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
                // A policy and the path it is checked against must be
                // expressed in the same terms. Script-supplied paths arrive
                // exactly as written while module resolution canonicalizes,
                // and on macOS `/tmp` and `/var/folders/...` are symlinks
                // into `/private`. Comparing raw would let a grant silently
                // fail to match, and — worse — let a deny entry be stepped
                // around by naming the other spelling of the same file.
                let resolved = canonical_policy_path(value);
                let value = resolved.as_deref().unwrap_or(value);
                let covers = |entry: &PathBuf| {
                    if value.starts_with(entry) {
                        return true;
                    }
                    canonical_policy_path(entry).is_some_and(|entry| value.starts_with(entry))
                };
                if deny_list.iter().any(&covers) {
                    return false;
                }
                allow_list.iter().any(covers)
            }
            Self::Intersect { outer, inner } => {
                outer.matches_path(value) && inner.matches_path(value)
            }
        }
    }
}

/// Resolve `value` to the form path policies are stored in.
///
/// Falls back to the longest existing ancestor with the remainder appended,
/// so a path that does not exist yet — the target of a first write — still
/// normalizes its existing prefix. Returns `None` when nothing resolves and
/// the caller should compare the path as written.
fn canonical_policy_path(value: &Path) -> Option<PathBuf> {
    if let Ok(path) = std::fs::canonicalize(value) {
        return Some(path);
    }
    let mut suffix = Vec::new();
    let mut cursor = value;
    while let Some(parent) = cursor.parent() {
        let name = cursor.file_name()?;
        suffix.push(name);
        if let Ok(mut path) = std::fs::canonicalize(parent) {
            for component in suffix.iter().rev() {
                path.push(component);
            }
            return Some(path);
        }
        cursor = parent;
    }
    None
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
    fn alternate_resource_spellings_preserve_deny_wins() {
        let bare_denied =
            Permission::<String>::allow_except(["*".to_string()], ["127.0.0.1".to_string()]);
        assert!(!bare_denied.matches_any(&["127.0.0.1", "127.0.0.1:8080"]));

        let port_denied = Permission::<String>::allow_except(
            ["example.test".to_string()],
            ["example.test:443".to_string()],
        );
        assert!(!port_denied.matches_any(&["example.test", "example.test:443"]));
    }

    #[test]
    fn capability_default_is_deny_by_default() {
        let caps = CapabilitySet::default();
        assert!(caps.read.is_deny());
        assert!(caps.write.is_deny());
        assert!(caps.net.is_deny());
        assert!(caps.env.is_deny());
        assert!(caps.run.is_deny());
        assert!(caps.ffi.is_deny());
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

#[cfg(test)]
mod host_global_tests {
    use super::*;

    #[test]
    fn builder_can_disable_product_host_globals() {
        let mut runtime = Runtime::builder()
            .process_global(false)
            .worker_global(false)
            .build()
            .expect("runtime");
        let completion = runtime
            .run_script(
                SourceInput::from_javascript("typeof process + ':' + typeof Worker;"),
                "<host-global-config>",
            )
            .expect("script")
            .completion_string()
            .to_string();
        assert_eq!(completion, "undefined:undefined");
    }
}
