//! Bounded, disposable on-disk cache of verified compiled bytecode.
//!
//! Bootstrap compilation dominates short-lived runtimes. This cache replaces
//! repeated compilation with one bounded flat-file read, while treating every
//! persistent byte as hostile until the bytecode decoder verifies it.
//!
//! # Contents
//! - [`CompileCache`] — cache lookup and publication policy.
//! - [`CompileCacheKey`] — the only value accepted by entry path construction.
//! - [`cache_key`] — a length-framed digest of every compiler input.
//! - `unix` — owner-private, descriptor-relative persistent storage.
//!
//! # Invariants
//! - Key components are individually length-prefixed. Embedded NUL bytes or
//!   component-boundary shifts cannot create the same preimage.
//! - Build time embeds a deterministic compiler/cache-schema fingerprint over
//!   sorted producer sources, manifests, the dependency lock, toolchain, and
//!   target/profile/cfg/features. Runtime executable paths and metadata are
//!   never inspected, so replacing an installed binary cannot change the key
//!   of a process that is already running.
//! - Persistent cache I/O exists only on Unix, where the backend can require
//!   owner-private directories/files and use no-follow, nonblocking,
//!   descriptor-relative operations. Other platforms fail closed as misses
//!   and no-op stores until they have an equivalent backend.
//! - Encoding and reading both stop at [`MAX_COMPILE_CACHE_ENTRY_BYTES`]. A
//!   large module is never fully encoded and rejected afterwards.
//! - Every hit passes the mandatory bytecode verifier before it can reach the
//!   VM. Every cache failure degrades silently to normal compilation.
//! - The cache is owner-trusted, cooperative storage, not a semantic
//!   authenticity boundary against another process running as the same user.
//!   The verifier protects VM safety; the private root, build fingerprint,
//!   and key prevent accidental cross-build reuse but do not authenticate that
//!   verified bytecode still corresponds to its source preimage.
//!
//! # See also
//! - [`otter_bytecode::binary`] for bounded encoding and verified decoding.

use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;

use otter_bytecode::{BytecodeModule, VerifiedBytecodeModule};
use otter_syntax::SourceKind;

#[cfg(unix)]
mod unix;

/// Subdirectory holding compiled entries.
#[cfg(unix)]
const CACHE_DIRECTORY: &str = "compiled";

/// Default hard quota for retained bytes in one compile-cache root.
pub(crate) const MAX_COMPILE_CACHE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Default hard quota for retained entries in one compile-cache root.
pub(crate) const MAX_COMPILE_CACHE_ENTRIES: usize = 4_096;

/// Maximum directory entries one maintenance pass will inspect.
///
/// A larger tree is treated as incomplete and cannot authorize publication.
#[cfg(unix)]
const MAX_COMPILE_CACHE_SCAN_ENTRIES: usize = 65_536;

/// Lowercase hexadecimal length of a BLAKE3 digest.
const COMPILE_CACHE_KEY_BYTES: usize = 64;

/// Build-time compiler, bytecode-format, and cache-schema identity.
const COMPILER_CACHE_FINGERPRINT: &str = env!("OTTER_COMPILER_CACHE_FINGERPRINT");

/// Largest encoded module the cache will produce or admit from disk.
///
/// The decoder budgets 16 bytes of decoded allocation per input byte plus a
/// 64 KiB floor, capped at 256 MiB. This inverse keeps both bounded encoding
/// and bounded I/O inside that admission envelope.
pub(crate) const MAX_COMPILE_CACHE_ENTRY_BYTES: usize = (256 * 1024 * 1024 - 64 * 1024) / 16;

static USER_DEFAULT_CACHE: OnceLock<Option<CompileCache>> = OnceLock::new();

/// Proof that a compile-cache key is one canonical BLAKE3 hex digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CompileCacheKey(Box<str>);

impl CompileCacheKey {
    fn from_hash(hash: blake3::Hash) -> Self {
        Self(hash.to_hex().as_str().into())
    }

    /// Canonical lowercase hexadecimal spelling.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A string was not one canonical compile-cache digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvalidCompileCacheKey;

impl std::fmt::Display for InvalidCompileCacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "compile-cache key must be exactly 64 lowercase hexadecimal bytes"
        )
    }
}

impl std::error::Error for InvalidCompileCacheKey {}

impl FromStr for CompileCacheKey {
    type Err = InvalidCompileCacheKey;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != COMPILE_CACHE_KEY_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(InvalidCompileCacheKey);
        }
        Ok(Self(value.into()))
    }
}

impl TryFrom<&str> for CompileCacheKey {
    type Error = InvalidCompileCacheKey;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// On-disk cache of compiled bytecode.
#[derive(Debug, Clone)]
pub(crate) struct CompileCache {
    root: PathBuf,
    #[cfg(unix)]
    policy: CompileCachePolicy,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct CompileCachePolicy {
    max_bytes: u64,
    max_entries: usize,
    max_scan_entries: usize,
}

#[cfg(unix)]
impl Default for CompileCachePolicy {
    fn default() -> Self {
        Self {
            max_bytes: MAX_COMPILE_CACHE_TOTAL_BYTES,
            max_entries: MAX_COMPILE_CACHE_ENTRIES,
            max_scan_entries: MAX_COMPILE_CACHE_SCAN_ENTRIES,
        }
    }
}

impl CompileCache {
    /// Create a cache rooted at `root`.
    ///
    /// On non-Unix targets this value intentionally performs no persistent
    /// I/O until an equally confined backend exists.
    #[must_use]
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            #[cfg(unix)]
            policy: CompileCachePolicy::default(),
        }
    }

    #[cfg(all(unix, test))]
    fn with_policy(root: impl Into<PathBuf>, policy: CompileCachePolicy) -> Self {
        Self {
            root: root.into(),
            policy,
        }
    }

    /// Cache shared by runtimes for the current user and embedded compiler
    /// fingerprint.
    #[must_use]
    pub(crate) fn user_default() -> Option<Self> {
        USER_DEFAULT_CACHE
            .get_or_init(|| {
                #[cfg(unix)]
                {
                    let root = dirs::cache_dir()?.join("otter").join(CACHE_DIRECTORY);
                    Some(Self::new(root))
                }
                #[cfg(not(unix))]
                {
                    None
                }
            })
            .clone()
    }

    /// Path an already-validated key conceptually occupies.
    ///
    /// The persistent backend does not resolve this path: it splits the key
    /// and operates relative to validated directory descriptors.
    #[cfg(test)]
    fn entry_path(&self, key: &CompileCacheKey) -> PathBuf {
        let (prefix, rest) = key.as_str().split_at(2);
        self.root.join(prefix).join(rest)
    }

    /// Read and verify a compiled module stored under this exact key.
    #[must_use]
    pub(crate) fn load(&self, key: &CompileCacheKey) -> Option<VerifiedBytecodeModule> {
        #[cfg(unix)]
        {
            let bytes = unix::load(&self.root, key)?;
            otter_bytecode::binary::decode_module(&bytes).ok()
        }
        #[cfg(not(unix))]
        {
            let _ = (&self.root, key);
            None
        }
    }

    /// Store a compiled module under `key` when bounded encoding and a full
    /// locked quota pass both succeed.
    pub(crate) fn store(&self, key: &CompileCacheKey, module: &BytecodeModule) {
        #[cfg(unix)]
        {
            let Ok(bytes) = otter_bytecode::binary::encode_module_bounded(
                module,
                MAX_COMPILE_CACHE_ENTRY_BYTES,
            ) else {
                return;
            };
            unix::store(&self.root, self.policy, key, &bytes);
        }
        #[cfg(not(unix))]
        {
            let _ = (&self.root, key, module);
        }
    }
}

/// Digest key for these exact compiler inputs and the embedded build-time
/// compiler/cache-schema fingerprint.
#[must_use]
pub(crate) fn cache_key(source: &str, kind: SourceKind, specifier: &str) -> CompileCacheKey {
    cache_key_with_fingerprint(
        source,
        kind,
        specifier,
        COMPILER_CACHE_FINGERPRINT.as_bytes(),
    )
}

fn cache_key_with_fingerprint(
    source: &str,
    kind: SourceKind,
    specifier: &str,
    fingerprint: &[u8],
) -> CompileCacheKey {
    let mut hasher = blake3::Hasher::new();
    for component in [
        b"otter-compile-cache-key".as_slice(),
        env!("CARGO_PKG_VERSION").as_bytes(),
        fingerprint,
        source.as_bytes(),
        source_kind_name(kind).as_bytes(),
        specifier.as_bytes(),
    ] {
        hash_framed(&mut hasher, component);
    }
    CompileCacheKey::from_hash(hasher.finalize())
}

fn hash_framed(hasher: &mut blake3::Hasher, component: &[u8]) {
    hasher.update(&component.len().to_le_bytes());
    hasher.update(component);
}

const fn source_kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::JavaScript => "js",
        SourceKind::JavaScriptJsx => "jsx",
        SourceKind::TypeScript => "ts",
        SourceKind::TypeScriptJsx => "tsx",
    }
}

/// Read at most `limit` bytes, probing one additional byte to distinguish an
/// exact-size entry from one that exceeded or grew beyond its metadata.
fn read_bounded(mut reader: impl Read, size_hint: Option<u64>, limit: usize) -> Option<Vec<u8>> {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    if size_hint.is_some_and(|size| size > limit_u64) {
        return None;
    }
    let initial_capacity = size_hint
        .and_then(|size| usize::try_from(size).ok())
        .unwrap_or(0)
        .min(limit);
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(initial_capacity).ok()?;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let remaining = limit.checked_sub(bytes.len())?;
        let requested = chunk.len().min(remaining.saturating_add(1));
        let read = reader.read(&mut chunk[..requested]).ok()?;
        if read == 0 {
            return Some(bytes);
        }
        if read > remaining {
            return None;
        }
        bytes.try_reserve(read).ok()?;
        bytes.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RepeatingReader {
        bytes_read: usize,
    }

    impl Read for RepeatingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            buffer.fill(0xa5);
            self.bytes_read += buffer.len();
            Ok(buffer.len())
        }
    }

    fn key(source: &str, kind: SourceKind, specifier: &str) -> CompileCacheKey {
        cache_key(source, kind, specifier)
    }

    #[test]
    fn every_input_changes_the_key() {
        let base = key("1 + 1", SourceKind::JavaScript, "<a>");
        assert_eq!(base.as_str().len(), COMPILE_CACHE_KEY_BYTES);
        assert!(
            base.as_str()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        );
        assert_eq!(base, key("1 + 1", SourceKind::JavaScript, "<a>"));
        assert_ne!(base, key("1 + 2", SourceKind::JavaScript, "<a>"));
        assert_ne!(base, key("1 + 1", SourceKind::TypeScript, "<a>"));
        assert_ne!(base, key("1 + 1", SourceKind::JavaScript, "<b>"));
    }

    #[test]
    fn length_framing_prevents_embedded_nul_boundary_collisions() {
        let fingerprint = b"fixed compiler fingerprint";
        let first =
            cache_key_with_fingerprint("x", SourceKind::JavaScript, "a\0js\0b", fingerprint);
        let second =
            cache_key_with_fingerprint("x\0js\0a", SourceKind::JavaScript, "b", fingerprint);
        assert_ne!(first, second);
    }

    #[test]
    fn embedded_compiler_fingerprint_is_one_canonical_digest() {
        assert_eq!(COMPILER_CACHE_FINGERPRINT.len(), COMPILE_CACHE_KEY_BYTES);
        assert!(
            COMPILER_CACHE_FINGERPRINT
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_runtime_image_path_cannot_change_the_embedded_key() {
        let directory = tempfile::tempdir().unwrap();
        let installed_path = directory.path().join("otter");
        let replacement = directory.path().join("otter.new");
        std::fs::write(&installed_path, b"old process image").unwrap();
        let old_process_fingerprint = b"fingerprint embedded in old image";
        let before = cache_key_with_fingerprint(
            "1 + 1",
            SourceKind::JavaScript,
            "<image-model>",
            old_process_fingerprint,
        );

        std::fs::write(&replacement, b"new process image").unwrap();
        std::fs::rename(&replacement, &installed_path).unwrap();
        let after = cache_key_with_fingerprint(
            "1 + 1",
            SourceKind::JavaScript,
            "<image-model>",
            old_process_fingerprint,
        );
        let next_process = cache_key_with_fingerprint(
            "1 + 1",
            SourceKind::JavaScript,
            "<image-model>",
            b"fingerprint embedded in new image",
        );

        assert_eq!(std::fs::read(installed_path).unwrap(), b"new process image");
        assert_eq!(before, after);
        assert_ne!(after, next_process);
    }

    #[test]
    fn only_exact_lowercase_hex_keys_can_reach_path_construction() {
        let traversal = format!("../{}", "a".repeat(61));
        let absolute = format!("/{}", "a".repeat(63));
        let uppercase = "A".repeat(COMPILE_CACHE_KEY_BYTES);
        let too_short = "a".repeat(COMPILE_CACHE_KEY_BYTES - 1);
        let too_long = "a".repeat(COMPILE_CACHE_KEY_BYTES + 1);
        let non_hex = "g".repeat(COMPILE_CACHE_KEY_BYTES);
        for invalid in [traversal, absolute, uppercase, too_short, too_long, non_hex] {
            assert!(CompileCacheKey::try_from(invalid.as_str()).is_err());
        }

        let cache = CompileCache::new("/cache-root");
        let valid =
            CompileCacheKey::try_from("a".repeat(COMPILE_CACHE_KEY_BYTES).as_str()).unwrap();
        assert_eq!(
            cache.entry_path(&valid),
            PathBuf::from("/cache-root")
                .join("aa")
                .join("a".repeat(COMPILE_CACHE_KEY_BYTES - 2))
        );
    }

    #[test]
    fn bounded_reader_rejects_oversized_metadata_and_growth() {
        const TEST_LIMIT: usize = 8;

        let mut oversized_hint = RepeatingReader::default();
        assert!(read_bounded(&mut oversized_hint, Some(9), TEST_LIMIT).is_none());
        assert_eq!(oversized_hint.bytes_read, 0);

        let mut grew_after_metadata = RepeatingReader::default();
        assert!(read_bounded(&mut grew_after_metadata, Some(1), TEST_LIMIT).is_none());
        assert_eq!(grew_after_metadata.bytes_read, TEST_LIMIT + 1);

        let exact = std::io::Cursor::new([0_u8; TEST_LIMIT]);
        assert_eq!(
            read_bounded(exact, Some(1), TEST_LIMIT).unwrap().len(),
            TEST_LIMIT
        );
    }
}
