//! Corpus traversal for the Test262 runner.
//!
//! Slice 101 ships only the bare walk: locate the
//! `vendor/test262/test/` tree, refuse to launch when it is missing,
//! and count `.js` files (excluding `_FIXTURE.js` per
//! [INTERPRETING §test files](https://github.com/tc39/test262/blob/main/INTERPRETING.md#test-files)).
//!
//! Slices 102 / 103 / 104 layer the metadata parser, per-test
//! driver, and shard supervisor on top of [`list_tests`].
//!
//! Spec link: <https://github.com/tc39/test262/blob/main/INTERPRETING.md>

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use thiserror::Error;

/// Resolved on-disk paths for a test262 checkout.
#[derive(Debug, Clone)]
pub struct CorpusPaths {
    /// Root of the submodule (`vendor/test262`).
    pub root: PathBuf,
    /// Test tree (`vendor/test262/test`).
    pub test_dir: PathBuf,
    /// Harness fragments (`vendor/test262/harness`).
    pub harness_dir: PathBuf,
}

/// Locate the test262 corpus on disk.
///
/// Per task 100 §"Source acquisition", `vendor/test262` is a
/// `git submodule`. Slice 101 refuses to run when the submodule is
/// missing or empty so the user gets an actionable error instead of
/// a near-empty baseline.
///
/// # Errors
/// - [`CorpusError::Missing`] when `vendor/test262` does not exist.
/// - [`CorpusError::Empty`] when `vendor/test262/test` is missing
///   or empty (uninitialised submodule).
pub fn ensure_corpus_present(repo_root: &Path) -> Result<CorpusPaths, CorpusError> {
    let root = repo_root.join("vendor").join("test262");
    if !root.exists() {
        return Err(CorpusError::Missing { root });
    }
    let test_dir = root.join("test");
    let harness_dir = root.join("harness");
    if !test_dir.is_dir() {
        return Err(CorpusError::Empty { root });
    }
    let mut entries = std::fs::read_dir(&test_dir).map_err(|e| CorpusError::Io {
        path: test_dir.clone(),
        message: e.to_string(),
    })?;
    if entries.next().is_none() {
        return Err(CorpusError::Empty { root });
    }
    Ok(CorpusPaths {
        root,
        test_dir,
        harness_dir,
    })
}

/// Walk the test262 `test/` tree and return every test path.
///
/// `_FIXTURE.js` files are excluded per
/// [INTERPRETING.md](https://github.com/tc39/test262/blob/main/INTERPRETING.md#test-files):
/// they are import targets used by other tests, not standalone
/// tests in their own right.
///
/// The walker honours `.gitignore` patterns inside the corpus via
/// [`ignore::WalkBuilder`] so newly-vendored fixtures cannot accidentally
/// blow up the count.
///
/// `filter` (when supplied) is a substring match on the path
/// relative to `paths.test_dir` — slice 101's CLI only wires the
/// `--filter <glob>` flag through as a substring; richer glob
/// support lands with slice 104.
pub fn list_tests(paths: &CorpusPaths, filter: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let walker = WalkBuilder::new(&paths.test_dir)
        .standard_filters(true)
        .git_ignore(true)
        .git_exclude(true)
        .hidden(false)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension() else {
            continue;
        };
        if ext != "js" {
            continue;
        }
        let path_str = path.to_string_lossy();
        if path_str.ends_with("_FIXTURE.js") {
            continue;
        }
        if let Some(filter) = filter {
            let rel = path
                .strip_prefix(&paths.test_dir)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            if !rel.contains(filter) {
                continue;
            }
        }
        out.push(path.to_path_buf());
    }
    out.sort();
    out
}

/// Convenience: same as [`list_tests`] but only returns the count.
#[must_use]
pub fn count_tests(paths: &CorpusPaths, filter: Option<&str>) -> usize {
    list_tests(paths, filter).len()
}

/// Errors raised by [`ensure_corpus_present`].
#[derive(Debug, Error)]
pub enum CorpusError {
    /// Submodule directory is missing entirely.
    #[error(
        "vendor/test262 is missing at {root:?}. Run: git submodule update --init --recursive vendor/test262"
    )]
    Missing {
        /// The expected submodule root.
        root: PathBuf,
    },
    /// Submodule is present but empty (uninitialised).
    #[error(
        "vendor/test262 is empty at {root:?} — the submodule is not initialised. Run: git submodule update --init --recursive vendor/test262"
    )]
    Empty {
        /// The submodule root.
        root: PathBuf,
    },
    /// I/O error while walking the corpus.
    #[error("io error reading {path:?}: {message}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying error message.
        message: String,
    },
}
