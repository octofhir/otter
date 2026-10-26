//! Package store: one unpacked tree per package tarball, shared by every
//! project on the machine.
//!
//! A tarball is unpacked once, into a directory named by the tarball's
//! integrity, and that directory never changes again. Installing a package is
//! a copy of its tree into the project: one copy-on-write clone where the
//! filesystem clones directories (APFS), a hard link per file where it does
//! not, and a byte copy where the store and the project share no filesystem.
//! No step reads, hashes or rewrites a stored file.
//!
//! # Contents
//! - [`TreeStore`] — the store, its trees and their materialization.
//! - [`StagedFile`] — one file of a tree being unpacked.
//!
//! # Invariants
//! - A tree is written into a staging directory private to its writer and
//!   published by one rename: a published tree is complete, and its existence
//!   is its only record.
//! - A published tree is immutable; a project's copy of it is the project's
//!   own (a clone shares blocks until written, a hard link shares the inode
//!   with every other project's link).
//! - A staged file is written once, already carrying its final mode; files
//!   of one tree may be written from several threads, each into a directory
//!   created before it.
//!
//! # See also
//! - [`crate::install`] for unpacking tarballs into the store and installing
//!   from it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::PackageManagerError;

/// Files a single thread writes before another one is worth starting.
const FILES_PER_WRITER: usize = 128;

/// The most threads one tree is written by.
const MAX_WRITERS: usize = 8;

/// One file of a tree being unpacked.
#[derive(Debug)]
pub(crate) struct StagedFile {
    /// Package-relative path, `/`-separated.
    pub(crate) path: String,
    /// The file's bytes.
    pub(crate) bytes: Vec<u8>,
    /// Whether it carries an executable bit.
    pub(crate) executable: bool,
}

/// The unpacked trees of every package tarball this machine installed.
#[derive(Debug, Clone)]
pub(crate) struct TreeStore {
    trees_root: PathBuf,
}

impl TreeStore {
    /// A store rooted at `root`.
    #[must_use]
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            trees_root: root.into().join("trees"),
        }
    }

    /// The tree of the tarball filed under `key`.
    #[must_use]
    pub(crate) fn tree(&self, key: &str) -> PathBuf {
        self.trees_root.join(key)
    }

    /// Whether the tarball filed under `key` is already unpacked.
    #[must_use]
    pub(crate) fn has_tree(&self, key: &str) -> bool {
        self.tree(key).is_dir()
    }

    /// Unpack `files` as the tree of `key`, unless another writer published
    /// it first.
    ///
    /// # Errors
    /// A filesystem failure writing the staged tree or publishing it.
    pub(crate) fn store_tree(
        &self,
        key: &str,
        files: Vec<StagedFile>,
    ) -> Result<(), PackageManagerError> {
        let staging = self.trees_root.join(format!(
            ".tmp-{key}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|err| io_error(&staging, &err))?;
        }
        create_dir_all(&staging)?;
        write_files(&staging, files)?;
        let tree = self.tree(key);
        match std::fs::rename(&staging, &tree) {
            Ok(()) => Ok(()),
            // A concurrent install landed the same tree first.
            Err(_) if tree.is_dir() => {
                let _ = std::fs::remove_dir_all(&staging);
                Ok(())
            }
            Err(err) => Err(io_error(&tree, &err)),
        }
    }

    /// Copy the tree of `key` to `target`, a path that does not exist yet.
    ///
    /// # Errors
    /// A missing tree, or a filesystem failure creating the copy.
    pub(crate) fn materialize(&self, key: &str, target: &Path) -> Result<(), PackageManagerError> {
        let tree = self.tree(key);
        if let Some(parent) = target.parent() {
            create_dir_all(parent)?;
        }
        #[cfg(target_os = "macos")]
        if reflink_copy::reflink(&tree, target).is_ok() {
            return Ok(());
        }
        link_tree(&tree, target)
    }
}

/// Write `files` under `root`, splitting a large tree across threads.
fn write_files(root: &Path, files: Vec<StagedFile>) -> Result<(), PackageManagerError> {
    let directories: BTreeSet<PathBuf> = files
        .iter()
        .filter_map(|file| Path::new(&file.path).parent().map(Path::to_path_buf))
        .filter(|parent| !parent.as_os_str().is_empty())
        .collect();
    for directory in &directories {
        create_dir_all(&root.join(directory))?;
    }
    let writers = files.len().div_ceil(FILES_PER_WRITER).clamp(1, MAX_WRITERS);
    if writers == 1 {
        return files.iter().try_for_each(|file| write_file(root, file));
    }
    let chunk = files.len().div_ceil(writers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|files| {
                scope.spawn(move || files.iter().try_for_each(|file| write_file(root, file)))
            })
            .collect();
        handles.into_iter().try_for_each(|handle| {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    })
}

fn write_file(root: &Path, file: &StagedFile) -> Result<(), PackageManagerError> {
    use std::io::Write as _;

    let target = file
        .path
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if file.executable { 0o755 } else { 0o644 });
    }
    options
        .open(&target)
        .and_then(|mut out| out.write_all(&file.bytes))
        .map_err(|err| io_error(&target, &err))
}

/// Recreate `source`'s directories under `target` and hard-link its files,
/// copying a file where the filesystem refuses the link.
fn link_tree(source: &Path, target: &Path) -> Result<(), PackageManagerError> {
    create_dir_all(target)?;
    let entries = std::fs::read_dir(source).map_err(|err| io_error(source, &err))?;
    for entry in entries {
        let entry = entry.map_err(|err| io_error(source, &err))?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type().map_err(|err| io_error(&from, &err))?;
        if kind.is_dir() {
            link_tree(&from, &to)?;
        } else if std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to).map_err(|err| io_error(&to, &err))?;
        }
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> Result<(), PackageManagerError> {
    std::fs::create_dir_all(path).map_err(|err| io_error(path, &err))
}

fn io_error(path: &Path, err: &std::io::Error) -> PackageManagerError {
    PackageManagerError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(path: &str, bytes: &[u8], executable: bool) -> StagedFile {
        StagedFile {
            path: path.to_string(),
            bytes: bytes.to_vec(),
            executable,
        }
    }

    #[test]
    fn a_stored_tree_materializes_with_its_files_and_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TreeStore::new(tmp.path().join("store"));
        store
            .store_tree(
                "pkg",
                vec![
                    staged("index.js", b"body", false),
                    staged("bin/tool.js", b"#!/usr/bin/env otter\n", true),
                ],
            )
            .unwrap();
        assert!(store.has_tree("pkg"));

        let target = tmp.path().join("node_modules/tool");
        store.materialize("pkg", &target).unwrap();
        assert_eq!(std::fs::read(target.join("index.js")).unwrap(), b"body");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&target.join("bin/tool.js")), 0o755);
            assert_eq!(mode(&target.join("index.js")), 0o644);
        }
    }

    #[test]
    fn a_large_tree_written_by_several_threads_is_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TreeStore::new(tmp.path().join("store"));
        let files = (0..FILES_PER_WRITER * 3)
            .map(|n| {
                staged(
                    &format!("lib/{}/f{n}.js", n % 7),
                    n.to_string().as_bytes(),
                    false,
                )
            })
            .collect();
        store.store_tree("big", files).unwrap();

        let target = tmp.path().join("out");
        store.materialize("big", &target).unwrap();
        for n in 0..FILES_PER_WRITER * 3 {
            let path = target.join(format!("lib/{}/f{n}.js", n % 7));
            assert_eq!(std::fs::read_to_string(path).unwrap(), n.to_string());
        }
    }

    #[test]
    fn a_tree_published_twice_keeps_the_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TreeStore::new(tmp.path().join("store"));
        store
            .store_tree("pkg", vec![staged("a.js", b"first", false)])
            .unwrap();
        store
            .store_tree("pkg", vec![staged("a.js", b"second", false)])
            .unwrap();
        assert_eq!(
            std::fs::read(store.tree("pkg").join("a.js")).unwrap(),
            b"first"
        );
    }
}
