//! Path extension module using the new #[dive] macro system.
//!
//! This module provides JS bindings for the path functions using the cleaner
//! `#[dive]` macro architecture. The JS wrapper is in a separate file (path.js).
//!
//! ## Architecture
//!
//! - `path.rs` - Pure Rust implementation of path functions
//! - `path_ext.rs` - Extension bindings using `#[dive(swift)]` macros
//! - `path.js` - JavaScript wrapper that calls native functions
//!
//! ## Example
//!
//! ```ignore
//! #[dive(swift)]
//! fn path_join(paths: Vec<String>) -> String {
//!     crate::path::join(&paths.iter().map(|s| s.as_str()).collect::<Vec<_>>())
//! }
//!
//! den!(path {
//!     dives: [path_join, path_resolve, path_dirname],
//!     js: "path.js",
//! });
//! ```

use otter_macros::dive;
use serde::{Deserialize, Serialize};

use crate::path::{self, ParsedPath as PathParsedPath};

/// Parsed path components returned by parse().
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedPath {
    pub root: String,
    pub dir: String,
    pub base: String,
    pub ext: String,
    pub name: String,
}

impl From<PathParsedPath> for ParsedPath {
    fn from(p: PathParsedPath) -> Self {
        Self {
            root: p.root.to_string(),
            dir: p.dir.to_string(),
            base: p.base.to_string(),
            ext: p.ext.to_string(),
            name: p.name.to_string(),
        }
    }
}

/// Format options for path.format().
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FormatOptions {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default)]
    pub name: String,
}

// ============================================================================
// Dive Functions - Each becomes a callable JS function
// ============================================================================

/// Join path segments with the platform-specific separator.
#[dive(swift)]
fn path_join(paths: Vec<String>) -> String {
    let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    path::join(&refs)
}

/// Resolve a sequence of paths to an absolute path.
#[dive(swift)]
fn path_resolve(paths: Vec<String>) -> String {
    let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
    path::resolve(&refs)
}

/// Get the directory name of a path.
#[dive(swift)]
fn path_dirname(p: String) -> String {
    path::dirname(&p).to_string()
}

/// Get the last portion of a path, optionally removing a suffix.
#[dive(swift)]
fn path_basename(p: String, suffix: Option<String>) -> String {
    path::basename(&p, suffix.as_deref()).to_string()
}

/// Get the extension of a path.
#[dive(swift)]
fn path_extname(p: String) -> String {
    path::extname(&p).to_string()
}

/// Check if a path is absolute.
#[dive(swift)]
fn path_is_absolute(p: String) -> bool {
    path::is_absolute(&p)
}

/// Normalize a path, resolving '..' and '.' segments.
#[dive(swift)]
fn path_normalize(p: String) -> String {
    path::normalize(&p)
}

/// Get the relative path from 'from' to 'to'.
#[dive(swift)]
fn path_relative(from: String, to: String) -> String {
    path::relative(&from, &to)
}

/// Parse a path into its components.
#[dive(swift)]
fn path_parse(p: String) -> ParsedPath {
    path::parse(&p).into()
}

/// Format a path from components.
#[dive(swift)]
fn path_format(options: FormatOptions) -> String {
    let parsed = PathParsedPath {
        root: options.root,
        dir: options.dir,
        base: options.base,
        ext: options.ext,
        name: options.name,
    };
    path::format(&parsed)
}

/// Get the platform-specific path separator.
#[dive(swift)]
fn path_sep() -> String {
    path::sep().to_string()
}

/// Get the platform-specific path delimiter.
#[dive(swift)]
fn path_delimiter() -> String {
    path::delimiter().to_string()
}

// ============================================================================
// Extension Creation - Using den! macro
// ============================================================================

// Note: We can't use den! macro directly here because it needs the JS file
// to exist at compile time via include_str!. Instead, we'll create the
// extension manually for now, but using the generated _dive_decl() functions.

/// Create the path extension using the new dive functions.
///
/// This replaces the old inline-JS approach with the cleaner
/// #[dive] macro architecture.
pub fn create_path_extension() -> otter_runtime::Extension {
    // JS code in a separate file - much cleaner than inline strings!
    let js_code = include_str!("path.js");

    otter_runtime::Extension::new("path")
        .with_ops(vec![
            path_join_dive_decl(),
            path_resolve_dive_decl(),
            path_dirname_dive_decl(),
            path_basename_dive_decl(),
            path_extname_dive_decl(),
            path_is_absolute_dive_decl(),
            path_normalize_dive_decl(),
            path_relative_dive_decl(),
            path_parse_dive_decl(),
            path_format_dive_decl(),
            path_sep_dive_decl(),
            path_delimiter_dive_decl(),
        ])
        .with_js(js_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_join() {
        let result = path_join(vec![
            "foo".to_string(),
            "bar".to_string(),
            "baz.txt".to_string(),
        ]);
        assert_eq!(result, "foo/bar/baz.txt");
    }

    #[test]
    fn test_path_dirname() {
        assert_eq!(path_dirname("/foo/bar/baz.txt".to_string()), "/foo/bar");
    }

    #[test]
    fn test_path_basename() {
        assert_eq!(
            path_basename("/foo/bar/baz.txt".to_string(), None),
            "baz.txt"
        );
        assert_eq!(
            path_basename("/foo/bar/baz.txt".to_string(), Some(".txt".to_string())),
            "baz"
        );
    }

    #[test]
    fn test_path_extname() {
        assert_eq!(path_extname("file.txt".to_string()), ".txt");
        assert_eq!(path_extname("file".to_string()), "");
    }

    #[test]
    fn test_path_parse() {
        let parsed = path_parse("/home/user/file.txt".to_string());
        assert_eq!(parsed.root, "/");
        assert_eq!(parsed.dir, "/home/user");
        assert_eq!(parsed.base, "file.txt");
        assert_eq!(parsed.ext, ".txt");
        assert_eq!(parsed.name, "file");
    }
}
