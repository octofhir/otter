//! Content-addressed package store.
//!
//! A package's files are stored once, by content hash, and every project that
//! installs that package links to the same bytes. Two checkouts of the same
//! dependency cost one copy on disk, not two; a version bump that touches one
//! file re-stores one file. The alternative — a full copy of every package
//! into every `node_modules` — pays the whole tree again each time.
//!
//! What a package *is* becomes an index: a map from relative path to the
//! content hash holding that path's bytes. Materializing is then a walk over
//! that index creating links, with no archive reading and no byte copying in
//! the common case.
//!
//! # Contents
//! - [`ContentStore`] — the hashed file store and its package indexes.
//! - [`PackageIndex`] — what one package's tree contains.
//! - [`StoredFile`] — one file's hash, size, and executable bit.
//!
//! # Invariants
//! - Stored content is immutable. A path under the store is named by the hash
//!   of its bytes, so writing it twice writes the same thing, and nothing ever
//!   updates a stored file in place.
//! - Writes land through a temporary path and a rename, so a reader never
//!   observes a partially written file or index.
//! - Materialization prefers a hard link and falls back to a copy. A link is
//!   free; a copy is correct on a filesystem that cannot link across its
//!   layout. Both leave the same tree behind.
//! - Index maps are ordered, so a fingerprint over an index is stable across
//!   platforms and runs.
//!
//! # See also
//! - [`crate::install`] for extraction into this store and materialization out
//!   of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::PackageManagerError;

/// One file inside a stored package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFile {
    /// Content hash naming this file's bytes in the store.
    pub hash: String,
    /// File size in bytes.
    pub size: u64,
    /// Whether the file carries an executable bit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
}

/// What one package's tree contains.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageIndex {
    /// Files keyed by package-relative path, always `/`-separated.
    pub files: BTreeMap<String, StoredFile>,
}

impl PackageIndex {
    /// Stable fingerprint of this index's contents.
    ///
    /// Two indexes fingerprint alike exactly when they would materialize the
    /// same tree, which is what makes it usable as an "already installed"
    /// marker.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        for (path, file) in &self.files {
            hasher.update(path.as_bytes());
            hasher.update([0]);
            hasher.update(file.hash.as_bytes());
            hasher.update([0]);
            hasher.update(u8::from(file.executable).to_le_bytes());
            hasher.update([0]);
        }
        hex_digest(&hasher.finalize())
    }
}

/// Hashed file store plus the package indexes that describe trees over it.
#[derive(Debug, Clone)]
pub struct ContentStore {
    files_root: PathBuf,
    index_root: PathBuf,
}

impl ContentStore {
    /// Create a store rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            files_root: root.join("files"),
            index_root: root.join("index"),
        }
    }

    /// Path holding the bytes named by `hash`.
    ///
    /// Content is split one level by hash prefix so a large store does not put
    /// every file in one directory.
    #[must_use]
    pub fn file_path(&self, hash: &str) -> PathBuf {
        let (prefix, rest) = hash.split_at(hash.len().min(2));
        self.files_root.join(prefix).join(rest)
    }

    /// Path holding the index for `key`.
    #[must_use]
    pub fn index_path(&self, key: &str) -> PathBuf {
        self.index_root.join(format!("{key}.json"))
    }

    /// Read a package index, if one was stored for `key`.
    pub async fn read_index(&self, key: &str) -> Result<Option<PackageIndex>, PackageManagerError> {
        let path = self.index_path(key);
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(PackageManagerError::Io {
                    path,
                    message: err.to_string(),
                });
            }
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| PackageManagerError::Archive {
                path,
                message: err.to_string(),
            })
    }

    /// Store `bytes` under their content hash and describe the result.
    ///
    /// Already-stored content is left untouched: the bytes cannot differ,
    /// because the path is named after them.
    pub fn add_file(
        &self,
        bytes: &[u8],
        executable: bool,
    ) -> Result<StoredFile, PackageManagerError> {
        let hash = hex_digest(&Sha256::digest(bytes));
        let path = self.file_path(&hash);
        if !path.exists() {
            let parent = path.parent().unwrap_or(&self.files_root);
            create_dir_all(parent)?;
            let temporary = parent.join(format!(".tmp-{hash}-{}", std::process::id()));
            std::fs::write(&temporary, bytes).map_err(|err| PackageManagerError::Io {
                path: temporary.clone(),
                message: err.to_string(),
            })?;
            set_executable(&temporary, executable)?;
            // A concurrent writer may have landed the same content first; the
            // bytes are identical either way, so a lost race is a success.
            match std::fs::rename(&temporary, &path) {
                Ok(()) => {}
                Err(_) if path.exists() => {
                    let _ = std::fs::remove_file(&temporary);
                }
                Err(err) => {
                    return Err(PackageManagerError::Io {
                        path,
                        message: err.to_string(),
                    });
                }
            }
        }
        Ok(StoredFile {
            hash,
            size: bytes.len() as u64,
            executable,
        })
    }

    /// Write the index describing one package tree.
    pub fn write_index(&self, key: &str, index: &PackageIndex) -> Result<(), PackageManagerError> {
        create_dir_all(&self.index_root)?;
        let path = self.index_path(key);
        let text = serde_json::to_string(index).map_err(|err| PackageManagerError::Archive {
            path: path.clone(),
            message: err.to_string(),
        })?;
        let temporary = self
            .index_root
            .join(format!(".tmp-{key}-{}.json", std::process::id()));
        std::fs::write(&temporary, text).map_err(|err| PackageManagerError::Io {
            path: temporary.clone(),
            message: err.to_string(),
        })?;
        std::fs::rename(&temporary, &path).map_err(|err| PackageManagerError::Io {
            path,
            message: err.to_string(),
        })
    }

    /// Materialize `index` into `target_root`, linking to stored content.
    pub fn materialize(
        &self,
        index: &PackageIndex,
        target_root: &Path,
    ) -> Result<(), PackageManagerError> {
        create_dir_all(target_root)?;
        for (relative, file) in &index.files {
            let target = relative
                .split('/')
                .fold(target_root.to_path_buf(), |path, part| path.join(part));
            if let Some(parent) = target.parent() {
                create_dir_all(parent)?;
            }
            match std::fs::remove_file(&target) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(PackageManagerError::Io {
                        path: target,
                        message: err.to_string(),
                    });
                }
            }
            let source = self.file_path(&file.hash);
            if std::fs::hard_link(&source, &target).is_ok() {
                continue;
            }
            // A filesystem that cannot link the store into this tree still has
            // to end up with the same file.
            std::fs::copy(&source, &target).map_err(|err| PackageManagerError::Io {
                path: target.clone(),
                message: err.to_string(),
            })?;
            set_executable(&target, file.executable)?;
        }
        Ok(())
    }
}

fn create_dir_all(path: &Path) -> Result<(), PackageManagerError> {
    std::fs::create_dir_all(path).map_err(|err| PackageManagerError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<(), PackageManagerError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|err| {
        PackageManagerError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        }
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<(), PackageManagerError> {
    Ok(())
}

fn hex_digest(digest: &[u8]) -> String {
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_is_stored_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(tmp.path());
        let first = store.add_file(b"same bytes", false).unwrap();
        let second = store.add_file(b"same bytes", false).unwrap();
        assert_eq!(first, second);
        assert!(store.file_path(&first.hash).is_file());
    }

    #[test]
    fn a_materialized_tree_shares_bytes_with_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(tmp.path().join("store"));
        let mut index = PackageIndex::default();
        index.files.insert(
            "index.js".to_string(),
            store.add_file(b"body", false).unwrap(),
        );
        index.files.insert(
            "bin/tool.js".to_string(),
            store.add_file(b"#!/usr/bin/env otter\n", true).unwrap(),
        );

        let target = tmp.path().join("node_modules/tool");
        store.materialize(&index, &target).unwrap();
        assert_eq!(std::fs::read(target.join("index.js")).unwrap(), b"body");
        assert!(target.join("bin/tool.js").is_file());

        let stored = store.file_path(&index.files["index.js"].hash);
        assert_eq!(
            std::fs::metadata(&stored).unwrap().len(),
            std::fs::metadata(target.join("index.js")).unwrap().len()
        );
    }

    #[test]
    fn materializing_twice_ends_at_the_same_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(tmp.path().join("store"));
        let mut index = PackageIndex::default();
        index.files.insert(
            "index.js".to_string(),
            store.add_file(b"one", false).unwrap(),
        );
        let target = tmp.path().join("out");
        store.materialize(&index, &target).unwrap();

        let mut next = PackageIndex::default();
        next.files.insert(
            "index.js".to_string(),
            store.add_file(b"two", false).unwrap(),
        );
        store.materialize(&next, &target).unwrap();
        assert_eq!(std::fs::read(target.join("index.js")).unwrap(), b"two");
    }

    #[tokio::test]
    async fn an_index_round_trips_through_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(tmp.path());
        let mut index = PackageIndex::default();
        index
            .files
            .insert("a.js".to_string(), store.add_file(b"a", false).unwrap());
        store.write_index("tool-1.0.0", &index).unwrap();
        assert_eq!(
            store.read_index("tool-1.0.0").await.unwrap(),
            Some(index.clone())
        );
        assert_eq!(store.read_index("absent").await.unwrap(), None);
    }

    #[test]
    fn a_fingerprint_follows_the_contents_not_the_order_they_were_added() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ContentStore::new(tmp.path());
        let a = store.add_file(b"a", false).unwrap();
        let b = store.add_file(b"b", false).unwrap();

        let mut one = PackageIndex::default();
        one.files.insert("a.js".to_string(), a.clone());
        one.files.insert("b.js".to_string(), b.clone());
        let mut two = PackageIndex::default();
        two.files.insert("b.js".to_string(), b);
        two.files.insert("a.js".to_string(), a);
        assert_eq!(one.fingerprint(), two.fingerprint());

        let mut different = one.clone();
        different.files.remove("a.js");
        assert_ne!(one.fingerprint(), different.fingerprint());
    }
}
