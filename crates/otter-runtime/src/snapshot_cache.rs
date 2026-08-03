//! On-disk cache of serialized isolate snapshots.
//!
//! A runtime build spends its startup constructing the same bootstrap
//! graph every launch; the snapshot blob turns that into a file read
//! plus a page restore. The blob is only meaningful to the exact
//! binary that wrote it — entry-point addresses serialize as
//! image-relative values and the bytecode encoding is unversioned —
//! so the build identity is hashed into every entry's *name*: a
//! rebuilt binary never finds a stale entry, only an absent one.
//!
//! # Contents
//! - [`SnapshotCache`] — the cache directory and its lookups.
//! - [`snapshot_cache_key`] — the name an entry gets.
//!
//! # Invariants
//! - The key preimage is NUL-separated and order-fixed: otter version,
//!   build identity, then the host-supplied surface tag describing
//!   which globals the build installs.
//! - Entries are written through a temporary file and a rename, so a
//!   reader never sees a half-written entry.
//! - Every failure degrades to bootstrapping. The cache is an
//!   optimization and may never be the reason a program does not run.
//!
//! # See also
//! - [`crate::compile_cache`] — the same discipline for bytecode.
//! - `otter-vm/src/snapshot.rs` — the blob this stores.

use std::path::PathBuf;

/// Subdirectory holding snapshot entries.
const CACHE_DIRECTORY: &str = "snapshots";

/// On-disk cache of serialized isolate snapshots.
#[derive(Debug, Clone)]
pub struct SnapshotCache {
    root: PathBuf,
}

impl SnapshotCache {
    /// Create a cache rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The cache shared by every runtime this user starts, when the
    /// platform offers a cache directory to put it in.
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

    /// Read a blob back, if this exact key was stored.
    #[must_use]
    pub fn load(&self, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.entry_path(key)).ok()
    }

    /// Store a blob under `key`.
    ///
    /// Failure is silent by design: a cache that cannot be written is
    /// a runtime that bootstraps, not a runtime that stops.
    pub fn store(&self, key: &str, bytes: &[u8]) {
        let path = self.entry_path(key);
        let Some(parent) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let temporary = parent.join(format!(".tmp-{key}-{}", std::process::id()));
        if std::fs::write(&temporary, bytes).is_err() {
            return;
        }
        if std::fs::rename(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
    }
}

/// Name for the snapshot a build with this surface produces.
///
/// `surface_tag` names everything about the build that shapes the
/// bootstrap graph and is not derivable from the binary itself: which
/// extension sets are installed, whether the process / worker globals
/// exist. Two builds differing in any of those must not share a blob.
#[must_use]
pub fn snapshot_cache_key(surface_tag: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    for component in [
        env!("CARGO_PKG_VERSION").as_bytes(),
        crate::compile_cache::build_identity().as_bytes(),
        surface_tag.as_bytes(),
    ] {
        hasher.update(component);
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_bytes_under_a_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = SnapshotCache::new(dir.path());
        let key = snapshot_cache_key("test-surface");
        assert!(cache.load(&key).is_none());
        cache.store(&key, b"snapshot-bytes");
        assert_eq!(cache.load(&key).as_deref(), Some(&b"snapshot-bytes"[..]));
    }

    #[test]
    fn different_surfaces_get_disjoint_keys() {
        assert_ne!(snapshot_cache_key("a"), snapshot_cache_key("b"));
    }
}
