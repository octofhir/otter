//! Path module - Node.js-compatible path manipulation
//!
//! Pure path utilities, no I/O.

use serde_json::{Value as JsonValue, json};
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

/// Join path segments
fn path_join(args: &[JsonValue]) -> Result<JsonValue, String> {
    let segments: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();

    if segments.is_empty() {
        return Ok(json!("."));
    }

    let mut path = PathBuf::new();
    for segment in segments {
        if segment.starts_with('/') || segment.starts_with('\\') {
            // Absolute segment resets the path
            path = PathBuf::from(segment);
        } else {
            path.push(segment);
        }
    }

    Ok(json!(normalize_path_string(&path.to_string_lossy())))
}

/// Resolve path to absolute
fn path_resolve(args: &[JsonValue]) -> Result<JsonValue, String> {
    let segments: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();

    let mut path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));

    for segment in segments {
        if Path::new(segment).is_absolute() {
            path = PathBuf::from(segment);
        } else {
            path.push(segment);
        }
    }

    // Canonicalize if possible, otherwise just normalize
    let result = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());

    Ok(json!(result.to_string_lossy()))
}

/// Get directory name
fn path_dirname(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("dirname requires path argument")?;

    let parent = Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());

    Ok(json!(if parent.is_empty() { "." } else { &parent }))
}

/// Get base name
fn path_basename(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("basename requires path argument")?;

    let ext = args.get(1).and_then(|v| v.as_str());

    let name = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // Strip extension if provided
    let result = if let Some(ext) = ext {
        if name.ends_with(ext) {
            name[..name.len() - ext.len()].to_string()
        } else {
            name
        }
    } else {
        name
    };

    Ok(json!(result))
}

/// Get file extension
fn path_extname(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("extname requires path argument")?;

    let ext = Path::new(path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    Ok(json!(ext))
}

/// Normalize path
fn path_normalize(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("normalize requires path argument")?;

    Ok(json!(normalize_path_string(path)))
}

/// Check if path is absolute
fn path_is_absolute(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("isAbsolute requires path argument")?;

    Ok(json!(Path::new(path).is_absolute()))
}

/// Parse path into components
fn path_parse(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("parse requires path argument")?;

    let p = Path::new(path);
    let dir = p.parent().map(|d| d.to_string_lossy()).unwrap_or_default();
    let base = p
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let name = p
        .file_stem()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();

    // Determine root
    let root = if cfg!(windows) {
        // Windows: extract drive letter if present
        if path.len() >= 2 && path.chars().nth(1) == Some(':') {
            format!("{}\\", &path[..2])
        } else if path.starts_with("\\\\") || path.starts_with("//") {
            "\\\\".to_string()
        } else {
            String::new()
        }
    } else if path.starts_with('/') {
        "/".to_string()
    } else {
        String::new()
    };

    Ok(json!({
        "root": root,
        "dir": dir,
        "base": base,
        "ext": ext,
        "name": name
    }))
}

/// Format path from components
fn path_format(args: &[JsonValue]) -> Result<JsonValue, String> {
    let obj = args.first().ok_or("format requires object argument")?;

    // If dir is provided, use it; otherwise use root
    let dir = obj.get("dir").and_then(|v| v.as_str());
    let root = obj.get("root").and_then(|v| v.as_str()).unwrap_or("");
    let base = obj.get("base").and_then(|v| v.as_str());
    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let ext = obj.get("ext").and_then(|v| v.as_str()).unwrap_or("");

    let base_part = base
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{name}{ext}"));

    let result = if let Some(dir) = dir {
        if dir.is_empty() {
            base_part
        } else {
            let ends_with_sep = dir.ends_with(MAIN_SEPARATOR);
            if ends_with_sep {
                format!("{dir}{base_part}")
            } else {
                format!("{dir}{MAIN_SEPARATOR}{base_part}")
            }
        }
    } else {
        format!("{root}{base_part}")
    };

    Ok(json!(result))
}

/// Get relative path from one to another
fn path_relative(args: &[JsonValue]) -> Result<JsonValue, String> {
    let from = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("relative requires from argument")?;

    let to = args
        .get(1)
        .and_then(|v| v.as_str())
        .ok_or("relative requires to argument")?;

    let normalize = |p: &str| -> PathBuf {
        let input = Path::new(p);
        let base = if input.is_absolute() {
            input.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(input)
        };
        dunce::canonicalize(&base).unwrap_or(base)
    };

    let from_path = normalize(from);
    let to_path = normalize(to);

    let from_components: Vec<_> = from_path.components().collect();
    let to_components: Vec<_> = to_path.components().collect();

    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    if common == 0 {
        return Ok(json!(to));
    }

    let mut rel = PathBuf::new();
    for _ in common..from_components.len() {
        rel.push("..");
    }
    for component in to_components.iter().skip(common) {
        rel.push(component.as_os_str());
    }

    if rel.as_os_str().is_empty() {
        Ok(json!(""))
    } else {
        Ok(json!(rel.to_string_lossy()))
    }
}

/// Get path separator
fn path_sep(_args: &[JsonValue]) -> Result<JsonValue, String> {
    Ok(json!(MAIN_SEPARATOR.to_string()))
}

/// Get path delimiter (for PATH env variable)
fn path_delimiter(_args: &[JsonValue]) -> Result<JsonValue, String> {
    let delimiter = if cfg!(windows) { ";" } else { ":" };
    Ok(json!(delimiter))
}

/// Normalize a path string (resolve . and ..)
fn normalize_path_string(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let is_absolute = path.starts_with('/') || path.starts_with('\\');

    for part in path.split(|c| c == '/' || c == '\\') {
        match part {
            "" | "." => continue,
            ".." => {
                if !parts.is_empty() && parts.last() != Some(&"..") {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }

    let result = parts.join(&MAIN_SEPARATOR.to_string());
    if is_absolute {
        format!("{}{}", MAIN_SEPARATOR, result)
    } else if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_join() {
        let result = path_join(&[json!("foo"), json!("bar"), json!("baz.js")]).unwrap();
        assert!(result.as_str().unwrap().contains("foo"));
    }

    #[test]
    fn test_path_dirname() {
        let result = path_dirname(&[json!("/foo/bar/baz.js")]).unwrap();
        assert_eq!(result, json!("/foo/bar"));
    }

    #[test]
    fn test_path_basename() {
        let result = path_basename(&[json!("/foo/bar/baz.js")]).unwrap();
        assert_eq!(result, json!("baz.js"));

        let result = path_basename(&[json!("/foo/bar/baz.js"), json!(".js")]).unwrap();
        assert_eq!(result, json!("baz"));
    }

    #[test]
    fn test_path_extname() {
        let result = path_extname(&[json!("/foo/bar/baz.js")]).unwrap();
        assert_eq!(result, json!(".js"));
    }

    #[test]
    fn test_path_is_absolute() {
        let result = path_is_absolute(&[json!("/foo/bar")]).unwrap();
        assert_eq!(result, json!(true));

        let result = path_is_absolute(&[json!("foo/bar")]).unwrap();
        assert_eq!(result, json!(false));
    }
}
