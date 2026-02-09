//! File system module - Node.js-compatible fs operations
//!
//! All operations are gated by fs_read/fs_write capabilities.

use otter_vm_runtime::extension::{op_async, op_sync, Op};
use serde_json::{json, Value as JsonValue};
use std::fs;
use std::path::Path;

/// Create fs native operations
pub fn fs_ops() -> Vec<Op> {
    vec![
        // Sync operations
        op_sync("__fs_read_file_sync", fs_read_file_sync),
        op_sync("__fs_write_file_sync", fs_write_file_sync),
        op_sync("__fs_exists_sync", fs_exists_sync),
        op_sync("__fs_stat_sync", fs_stat_sync),
        op_sync("__fs_readdir_sync", fs_readdir_sync),
        op_sync("__fs_mkdir_sync", fs_mkdir_sync),
        op_sync("__fs_rmdir_sync", fs_rmdir_sync),
        op_sync("__fs_unlink_sync", fs_unlink_sync),
        // Async operations
        op_async("__fs_read_file", fs_read_file_async),
        op_async("__fs_write_file", fs_write_file_async),
        op_async("__fs_stat", fs_stat_async),
    ]
}

/// Read file synchronously
fn fs_read_file_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("readFileSync requires path argument")?;

    let encoding = args.get(1).and_then(|v| {
        if v.is_string() {
            v.as_str()
        } else {
            v.get("encoding").and_then(|e| e.as_str())
        }
    });

    let bytes = fs::read(path).map_err(|e| format!("ENOENT: {}", e))?;

    match encoding {
        Some("utf8") | Some("utf-8") => {
            let content = String::from_utf8_lossy(&bytes);
            Ok(json!(content))
        }
        Some(enc) => Err(format!("Unknown encoding: {}", enc)),
        None => {
            // Return as array of bytes (Buffer-like)
            Ok(json!(bytes))
        }
    }
}

/// Write file synchronously
fn fs_write_file_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("writeFileSync requires path argument")?;

    let data = args.get(1).ok_or("writeFileSync requires data argument")?;

    let bytes: Vec<u8> = if let Some(s) = data.as_str() {
        s.as_bytes().to_vec()
    } else if let Some(arr) = data.as_array() {
        arr.iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect()
    } else {
        return Err("Data must be string or byte array".to_string());
    };

    fs::write(path, bytes).map_err(|e| format!("ENOENT: {}", e))?;
    Ok(json!(null))
}

/// Check if file exists
fn fs_exists_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("existsSync requires path argument")?;

    Ok(json!(Path::new(path).exists()))
}

/// Get file stats
fn fs_stat_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("statSync requires path argument")?;

    let metadata = fs::metadata(path).map_err(|e| format!("ENOENT: {}", e))?;

    Ok(json!({
        "isFile": metadata.is_file(),
        "isDirectory": metadata.is_dir(),
        "isSymbolicLink": metadata.is_symlink(),
        "size": metadata.len(),
        "mode": 0o644, // Simplified
    }))
}

/// Read directory contents
fn fs_readdir_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("readdirSync requires path argument")?;

    let entries: Vec<String> = fs::read_dir(path)
        .map_err(|e| format!("ENOENT: {}", e))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();

    Ok(json!(entries))
}

/// Create directory
fn fs_mkdir_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("mkdirSync requires path argument")?;

    let recursive = args
        .get(1)
        .and_then(|v| v.get("recursive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if recursive {
        fs::create_dir_all(path).map_err(|e| format!("ENOENT: {}", e))?;
    } else {
        fs::create_dir(path).map_err(|e| format!("ENOENT: {}", e))?;
    }

    Ok(json!(null))
}

/// Remove directory
fn fs_rmdir_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("rmdirSync requires path argument")?;

    let recursive = args
        .get(1)
        .and_then(|v| v.get("recursive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if recursive {
        fs::remove_dir_all(path).map_err(|e| format!("ENOENT: {}", e))?;
    } else {
        fs::remove_dir(path).map_err(|e| format!("ENOENT: {}", e))?;
    }

    Ok(json!(null))
}

/// Delete file
fn fs_unlink_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("unlinkSync requires path argument")?;

    fs::remove_file(path).map_err(|e| format!("ENOENT: {}", e))?;
    Ok(json!(null))
}

/// Read file async
fn fs_read_file_async(
    args: &[JsonValue],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsonValue, String>> + Send>> {
    let args = args.to_vec();
    Box::pin(async move { fs_read_file_sync(&args) })
}

/// Write file async
fn fs_write_file_async(
    args: &[JsonValue],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsonValue, String>> + Send>> {
    let args = args.to_vec();
    Box::pin(async move { fs_write_file_sync(&args) })
}

/// Stat async
fn fs_stat_async(
    args: &[JsonValue],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsonValue, String>> + Send>> {
    let args = args.to_vec();
    Box::pin(async move { fs_stat_sync(&args) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_fs_read_write() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");

        // Write
        let path_str = file_path.to_string_lossy().to_string();
        let result = fs_write_file_sync(&[json!(path_str), json!("Hello, World!")]);
        assert!(result.is_ok());

        // Read as utf8
        let result = fs_read_file_sync(&[json!(path_str), json!("utf8")]).unwrap();
        assert_eq!(result, json!("Hello, World!"));
    }

    #[test]
    fn test_fs_exists() {
        let result = fs_exists_sync(&[json!("/nonexistent/file")]).unwrap();
        assert_eq!(result, json!(false));
    }
}
