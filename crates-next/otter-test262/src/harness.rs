//! Harness preamble loader (`assert.js` / `sta.js` / per-test
//! `includes`).
//!
//! Test262 ships a small JavaScript harness under
//! `vendor/test262/harness/` that every non-`raw` test relies on:
//! `assert.js` provides the `assert.sameValue` family, `sta.js`
//! ("Standard Test API") wires the `Test262Error` constructor, and
//! per-test `includes:` entries bring in helpers like
//! `compareArray.js` or `propertyHelper.js`.
//!
//! Slice 102 builds the preamble *string* for each test;
//! slice 103 routes it through `Runtime::run_script` on a fresh
//! runtime per test.
//!
//! Spec: <https://github.com/tc39/test262/blob/main/INTERPRETING.md#shell>

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::metadata::Frontmatter;

/// `$DONE` polyfill installed for `flags: [async]` tests.
///
/// Slice 102 wires the **stub** — slice 103 swaps `__OTTER_TEST262_DONE`
/// for a real native that signals the per-test driver. The shape
/// matches what test262's own `doneprintHandle.js` expects:
/// `$DONE()` for success, `$DONE(reason)` for failure (matching
/// `Promise` rejection semantics).
pub const DONE_POLYFILL_STUB: &str = r#"
var __OTTER_TEST262_DONE_RESULT = undefined;
var __OTTER_TEST262_DONE_FIRED = false;
function $DONE(reason) {
    if (__OTTER_TEST262_DONE_FIRED) { return; }
    __OTTER_TEST262_DONE_FIRED = true;
    __OTTER_TEST262_DONE_RESULT = reason;
}
"#;

/// Errors raised by the harness loader.
#[derive(Debug, Error)]
pub enum HarnessError {
    /// `vendor/test262/harness/<name>` did not exist or could not be
    /// read.
    #[error("harness fragment {include:?} not found at {path:?}: {message}")]
    MissingInclude {
        /// `includes:` token from the frontmatter.
        include: String,
        /// Resolved on-disk path.
        path: PathBuf,
        /// Underlying I/O error message.
        message: String,
    },
}

/// Harness fragment cache keyed by include name (e.g. `assert.js`).
///
/// Lives once per worker so the file system hit is paid at most
/// once per fragment in a full sweep. The runtime cost per test is
/// just a `HashMap` lookup + `&str` clone.
#[derive(Debug, Default)]
pub struct HarnessCache {
    harness_dir: PathBuf,
    fragments: HashMap<String, String>,
}

impl HarnessCache {
    /// Build a cache rooted at `vendor/test262/harness`.
    #[must_use]
    pub fn new(harness_dir: impl Into<PathBuf>) -> Self {
        Self {
            harness_dir: harness_dir.into(),
            fragments: HashMap::new(),
        }
    }

    /// Pre-warm `assert.js` and `sta.js` so the very first test
    /// doesn't pay two extra fs reads.
    ///
    /// # Errors
    /// Returns the first [`HarnessError::MissingInclude`] from the
    /// two reads.
    pub fn prewarm(&mut self) -> Result<(), HarnessError> {
        for name in ["assert.js", "sta.js"] {
            self.load(name)?;
        }
        Ok(())
    }

    /// Load and cache a single harness fragment.
    ///
    /// # Errors
    /// [`HarnessError::MissingInclude`] when the file does not
    /// exist or cannot be read.
    pub fn load(&mut self, include: &str) -> Result<&str, HarnessError> {
        if !self.fragments.contains_key(include) {
            let path = self.harness_dir.join(include);
            let text =
                std::fs::read_to_string(&path).map_err(|e| HarnessError::MissingInclude {
                    include: include.to_string(),
                    path: path.clone(),
                    message: e.to_string(),
                })?;
            self.fragments.insert(include.to_string(), text);
        }
        Ok(self.fragments.get(include).expect("just inserted").as_str())
    }

    /// Build the preamble string for a single test, in order:
    /// 1. `assert.js` (unless `flags: [raw]`)
    /// 2. `sta.js` (unless `flags: [raw]`)
    /// 3. Each `includes:` entry in the order it appears.
    /// 4. `$DONE` polyfill stub when `flags: [async]`.
    ///
    /// `flags: [raw]` returns an empty string.
    ///
    /// # Errors
    /// First [`HarnessError::MissingInclude`] from any included
    /// fragment.
    pub fn preamble_for(&mut self, fm: &Frontmatter) -> Result<String, HarnessError> {
        if fm.is_raw() {
            return Ok(String::new());
        }
        let mut out = String::with_capacity(8 * 1024);
        for required in ["assert.js", "sta.js"] {
            out.push_str(self.load(required)?);
            out.push('\n');
        }
        for include in &fm.includes {
            out.push_str(self.load(include.as_str())?);
            out.push('\n');
        }
        if fm.is_async() {
            out.push_str(DONE_POLYFILL_STUB);
            out.push('\n');
        }
        Ok(out)
    }

    /// Borrow the harness dir.
    #[must_use]
    pub fn harness_dir(&self) -> &Path {
        &self.harness_dir
    }

    /// Number of cached fragments (for diagnostics).
    #[must_use]
    pub fn cached_count(&self) -> usize {
        self.fragments.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("assert.js"),
            "var assert = function () {};\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("sta.js"), "function Test262Error(){}\n").unwrap();
        std::fs::write(
            dir.path().join("propertyHelper.js"),
            "var propertyHelper = 1;\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn prewarm_loads_assert_and_sta() {
        let dir = mock_dir();
        let mut cache = HarnessCache::new(dir.path());
        cache.prewarm().unwrap();
        assert_eq!(cache.cached_count(), 2);
    }

    #[test]
    fn raw_skips_preamble() {
        let dir = mock_dir();
        let mut cache = HarnessCache::new(dir.path());
        let mut fm = Frontmatter::default();
        fm.flags.push("raw".to_string());
        let p = cache.preamble_for(&fm).unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn async_appends_done_polyfill() {
        let dir = mock_dir();
        let mut cache = HarnessCache::new(dir.path());
        let mut fm = Frontmatter::default();
        fm.flags.push("async".to_string());
        let p = cache.preamble_for(&fm).unwrap();
        assert!(p.contains("function $DONE"));
        assert!(p.contains("var assert"));
    }

    #[test]
    fn includes_load_in_order() {
        let dir = mock_dir();
        let mut cache = HarnessCache::new(dir.path());
        let mut fm = Frontmatter::default();
        fm.includes.push("propertyHelper.js".to_string());
        let p = cache.preamble_for(&fm).unwrap();
        assert!(p.contains("var propertyHelper"));
    }

    #[test]
    fn missing_include_errors() {
        let dir = mock_dir();
        let mut cache = HarnessCache::new(dir.path());
        let mut fm = Frontmatter::default();
        fm.includes.push("does-not-exist.js".to_string());
        let err = cache.preamble_for(&fm).unwrap_err();
        assert!(matches!(err, HarnessError::MissingInclude { .. }));
    }
}
