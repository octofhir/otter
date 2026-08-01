//! On-disk cache of compiled bytecode.
//!
//! Starting a runtime compiles the same bootstrap sources on every launch, and
//! for a short program that compile is most of what the process does. Reading
//! the bytecode back instead turns it into a file read and a linear decode.
//!
//! Reading has to be *much* cheaper than compiling, or the cache is a slower
//! way to reach the same state — a general-purpose serializer measured slower
//! here than the compiler it was meant to replace. Entries therefore use the
//! flat encoding in [`otter_bytecode::binary`], the shape shipped code caches
//! use: no value graph to walk, no allocation per node.
//!
//! The other risk is serving an entry that no longer matches the code that
//! produced it, because bytecode from an older build is shaped like bytecode
//! from this one and a mismatch surfaces as a wrong answer rather than an
//! error. So every input that could change the output is hashed into the
//! entry's *name*: a rebuilt binary, a changed source, a different parse goal
//! each yield a different filename. A stale entry is never found rather than
//! found and mis-read; nothing migrates and nothing is versioned.
//!
//! # Contents
//! - [`CompileCache`] — the cache directory and its lookups.
//! - [`cache_key`] — the name an entry gets, derived from every input.
//!
//! # Invariants
//! - The key preimage is NUL-separated and order-fixed. Every component is
//!   hashed in, so any change to any of them yields a disjoint name.
//! - The build identity comes from the running executable itself, so a rebuild
//!   invalidates without anyone remembering to bump a constant.
//! - Entries are written through a temporary file and a rename, so a reader
//!   never sees a half-written entry.
//! - Every failure — unreadable directory, corrupt entry, unwritable cache —
//!   degrades to compiling. The cache is an optimization and may never be the
//!   reason a program does not run.
//!
//! # See also
//! - [`otter_bytecode::binary`] for the entry encoding.

use std::path::{Path, PathBuf};

use otter_bytecode::BytecodeModule;
use otter_syntax::SourceKind;

/// Subdirectory holding compiled entries.
const CACHE_DIRECTORY: &str = "compiled";

/// On-disk cache of compiled bytecode.
#[derive(Debug, Clone)]
pub struct CompileCache {
    root: PathBuf,
}

impl CompileCache {
    /// Create a cache rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The cache shared by every runtime this user starts, when the platform
    /// offers a cache directory to put it in.
    #[must_use]
    pub fn user_default() -> Option<Self> {
        let root = dirs::cache_dir()?.join("otter").join(CACHE_DIRECTORY);
        Some(Self::new(root))
    }

    /// Path an entry is stored at.
    #[must_use]
    pub fn entry_path(&self, key: &str) -> PathBuf {
        let (prefix, rest) = key.split_at(key.len().min(2));
        self.root.join(prefix).join(rest)
    }

    /// Read a compiled module back, if this exact key was stored.
    #[must_use]
    pub fn load(&self, key: &str) -> Option<BytecodeModule> {
        let bytes = std::fs::read(self.entry_path(key)).ok()?;
        otter_bytecode::binary::decode_module(&bytes)
    }

    /// Store a compiled module under `key`.
    ///
    /// Failure is silent by design: a cache that cannot be written is a
    /// runtime that compiles, not a runtime that stops.
    pub fn store(&self, key: &str, module: &BytecodeModule) {
        let bytes = otter_bytecode::binary::encode_module(module);
        let path = self.entry_path(key);
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let temporary = parent.join(format!(".tmp-{key}-{}", std::process::id()));
        if std::fs::write(&temporary, &bytes).is_err() {
            return;
        }
        if std::fs::rename(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    }
}

/// Name for the entry compiled from these exact inputs.
///
/// The preimage is `otter_version \0 build_id \0 source \0 kind \0 specifier`,
/// NUL-separated. `specifier` is in the preimage because it is baked into the
/// compiled module, so two identical sources compiled under different
/// specifiers must not share an entry.
#[must_use]
pub fn cache_key(source: &str, kind: SourceKind, specifier: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    for component in [
        env!("CARGO_PKG_VERSION").as_bytes(),
        build_identity().as_bytes(),
        source.as_bytes(),
        source_kind_name(kind).as_bytes(),
        specifier.as_bytes(),
    ] {
        hasher.update(component);
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

/// Identity of the build that would produce the bytecode.
///
/// The running executable's own size and modification time change on every
/// rebuild, which is exactly the invalidation a hand-maintained constant keeps
/// forgetting to do. When the executable cannot be inspected the identity is
/// deliberately unrepeatable, so every process computes a different one and
/// nothing is ever reused — the safe way to be wrong.
fn build_identity() -> String {
    let Ok(path) = std::env::current_exe() else {
        return unrepeatable_identity();
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return unrepeatable_identity();
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("{}:{}:{modified}", executable_name(&path), metadata.len())
}

fn executable_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

fn unrepeatable_identity() -> String {
    format!("unidentified:{}", std::process::id())
}

const fn source_kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::JavaScript => "js",
        SourceKind::JavaScriptJsx => "jsx",
        SourceKind::TypeScript => "ts",
        SourceKind::TypeScriptJsx => "tsx",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(source: &str) -> BytecodeModule {
        otter_compiler::compile_script_source_to_module(source, SourceKind::JavaScript, "<test>")
            .expect("compile fixture")
            .bytecode
    }

    #[test]
    fn every_input_changes_the_key() {
        let base = cache_key("1 + 1", SourceKind::JavaScript, "<a>");
        assert_eq!(base, cache_key("1 + 1", SourceKind::JavaScript, "<a>"));
        assert_ne!(base, cache_key("1 + 2", SourceKind::JavaScript, "<a>"));
        assert_ne!(base, cache_key("1 + 1", SourceKind::TypeScript, "<a>"));
        assert_ne!(base, cache_key("1 + 1", SourceKind::JavaScript, "<b>"));
    }

    #[test]
    fn a_stored_module_reads_back_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = CompileCache::new(tmp.path());
        let key = cache_key("globalThis.x = 41 + 1;", SourceKind::JavaScript, "<test>");
        let module = compiled("globalThis.x = 41 + 1;");

        assert!(cache.load(&key).is_none());
        cache.store(&key, &module);
        let restored = cache.load(&key).expect("entry stored under this key");
        assert_eq!(
            otter_bytecode::dump::to_json_pretty(&restored).unwrap(),
            otter_bytecode::dump::to_json_pretty(&module).unwrap()
        );
    }

    #[test]
    fn a_key_nobody_stored_is_a_miss_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = CompileCache::new(tmp.path());
        assert!(
            cache
                .load(&cache_key("1", SourceKind::JavaScript, "<x>"))
                .is_none()
        );
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = CompileCache::new(tmp.path());
        let key = cache_key("1 + 1", SourceKind::JavaScript, "<test>");
        let path = cache.entry_path(&key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not bytecode").unwrap();
        assert!(cache.load(&key).is_none());
    }

    #[test]
    fn an_unwritable_cache_stores_nothing_and_says_nothing() {
        let cache = CompileCache::new(Path::new("/proc/nonexistent-otter-cache"));
        let key = cache_key("1 + 1", SourceKind::JavaScript, "<test>");
        cache.store(&key, &compiled("1 + 1"));
        assert!(cache.load(&key).is_none());
    }
}
