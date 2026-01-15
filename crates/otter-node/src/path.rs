//! node:path implementation
//!
//! Path manipulation utilities compatible with Node.js path module.
//! This is a pure computation module - no capabilities required.

use std::path::{MAIN_SEPARATOR, Path, PathBuf};

/// Join path segments.
pub fn join(paths: &[&str]) -> String {
    if paths.is_empty() {
        return ".".to_string();
    }

    let mut result = PathBuf::new();
    for p in paths {
        if p.starts_with('/') || (cfg!(windows) && p.len() >= 2 && p.chars().nth(1) == Some(':')) {
            // Absolute path resets
            result = PathBuf::from(p);
        } else {
            result.push(p);
        }
    }

    result.to_string_lossy().to_string()
}

/// Resolve paths to an absolute path.
pub fn resolve(paths: &[&str]) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut result = cwd;

    for p in paths {
        if p.starts_with('/') || (cfg!(windows) && p.len() >= 2 && p.chars().nth(1) == Some(':')) {
            result = PathBuf::from(p);
        } else {
            result.push(p);
        }
    }

    normalize_path(&result).to_string_lossy().to_string()
}

/// Get directory name.
pub fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| {
            let s = p.to_string_lossy().to_string();
            if s.is_empty() { ".".to_string() } else { s }
        })
        .unwrap_or_else(|| ".".to_string())
}

/// Get base name, optionally stripping suffix.
pub fn basename(path: &str, suffix: Option<&str>) -> String {
    let mut base = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    if let Some(suffix) = suffix
        && base.ends_with(suffix) {
            base = base[..base.len() - suffix.len()].to_string();
        }

    base
}

/// Get file extension.
pub fn extname(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default()
}

/// Check if path is absolute.
pub fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}

/// Normalize path by resolving . and .. components.
pub fn normalize(path: &str) -> String {
    normalize_path(Path::new(path))
        .to_string_lossy()
        .to_string()
}

/// Compute relative path from one location to another.
pub fn relative(from: &str, to: &str) -> String {
    let from_path = normalize_path(Path::new(from));
    let to_path = normalize_path(Path::new(to));

    pathdiff::diff_paths(&to_path, &from_path)
        .unwrap_or_else(|| PathBuf::from(to))
        .to_string_lossy()
        .to_string()
}

/// Parsed path components.
#[derive(Debug, Clone, Default)]
pub struct ParsedPath {
    pub root: String,
    pub dir: String,
    pub base: String,
    pub ext: String,
    pub name: String,
}

/// Parse path into components.
pub fn parse(path: &str) -> ParsedPath {
    let p = Path::new(path);

    let root = if p.is_absolute() {
        if cfg!(windows) {
            p.components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            "/".to_string()
        }
    } else {
        String::new()
    };

    let dir = p
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let base = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    let name = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    ParsedPath {
        root,
        dir,
        base,
        ext,
        name,
    }
}

/// Format path from components.
pub fn format(parsed: &ParsedPath) -> String {
    let filename = if !parsed.base.is_empty() {
        parsed.base.clone()
    } else {
        format!("{}{}", parsed.name, parsed.ext)
    };

    if !parsed.dir.is_empty() {
        format!("{}{}{}", parsed.dir, MAIN_SEPARATOR, filename)
    } else if !parsed.root.is_empty() {
        format!("{}{}", parsed.root, filename)
    } else {
        filename
    }
}

/// Platform path separator.
pub fn sep() -> char {
    MAIN_SEPARATOR
}

/// Platform PATH delimiter.
pub fn delimiter() -> char {
    if cfg!(windows) { ';' } else { ':' }
}

/// Normalize a path by resolving . and .. components.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !components.is_empty() {
                    components.pop();
                }
            }
            c => components.push(c),
        }
    }

    if components.is_empty() {
        PathBuf::from(".")
    } else {
        components.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_join() {
        assert_eq!(join(&["foo", "bar", "baz"]), "foo/bar/baz");
        assert_eq!(join(&["foo", "/bar", "baz"]), "/bar/baz");
        assert_eq!(join(&[]), ".");
    }

    #[test]
    fn test_dirname() {
        assert_eq!(dirname("/foo/bar/baz.txt"), "/foo/bar");
        assert_eq!(dirname("baz.txt"), ".");
        assert_eq!(dirname("/foo"), "/");
    }

    #[test]
    fn test_basename() {
        assert_eq!(basename("/foo/bar/baz.txt", None), "baz.txt");
        assert_eq!(basename("/foo/bar/baz.txt", Some(".txt")), "baz");
        assert_eq!(basename("/foo/bar/baz.txt", Some(".md")), "baz.txt");
    }

    #[test]
    fn test_extname() {
        assert_eq!(extname("file.txt"), ".txt");
        assert_eq!(extname("file.test.txt"), ".txt");
        assert_eq!(extname("file"), "");
        assert_eq!(extname(".hidden"), "");
    }

    #[test]
    fn test_is_absolute() {
        assert!(is_absolute("/foo/bar"));
        assert!(!is_absolute("foo/bar"));
        assert!(!is_absolute("./foo"));
    }

    #[test]
    fn test_normalize() {
        assert_eq!(normalize("/foo/bar/../baz"), "/foo/baz");
        assert_eq!(normalize("./foo/./bar"), "foo/bar");
        assert_eq!(normalize("foo/../bar"), "bar");
    }

    #[test]
    fn test_parse() {
        let parsed = parse("/home/user/file.txt");
        assert_eq!(parsed.root, "/");
        assert_eq!(parsed.dir, "/home/user");
        assert_eq!(parsed.base, "file.txt");
        assert_eq!(parsed.ext, ".txt");
        assert_eq!(parsed.name, "file");
    }

    #[test]
    fn test_format() {
        let parsed = ParsedPath {
            root: "/".to_string(),
            dir: "/home/user".to_string(),
            base: "file.txt".to_string(),
            ext: ".txt".to_string(),
            name: "file".to_string(),
        };
        assert_eq!(format(&parsed), "/home/user/file.txt");
    }

    #[test]
    fn test_sep() {
        assert!(sep() == '/' || sep() == '\\');
    }

    #[test]
    fn test_delimiter() {
        assert!(delimiter() == ':' || delimiter() == ';');
    }
}
