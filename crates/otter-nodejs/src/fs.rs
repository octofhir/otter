//! File system module - Node.js-compatible fs operations.
//!
//! Security model:
//! - Read operations require `fs_read`.
//! - Write/mutation operations require `fs_write`.
//! - Checks are fail-closed at Rust boundary.

use crate::security;
use serde_json::{Value as JsonValue, json};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const F_OK: u64 = 0;
const X_OK: u64 = 1;
const W_OK: u64 = 2;
const R_OK: u64 = 4;

static MKDTEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn get_required_string<'a>(args: &'a [JsonValue], idx: usize, op: &str) -> Result<&'a str, String> {
    args.get(idx)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{op} requires argument at index {idx}"))
}

fn parse_encoding(value: Option<&JsonValue>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };

    if let Some(encoding) = value.as_str() {
        return Ok(Some(encoding.to_string()));
    }

    if let Some(encoding) = value.get("encoding").and_then(|v| v.as_str()) {
        return Ok(Some(encoding.to_string()));
    }

    if value.is_null() {
        return Ok(None);
    }

    Err("Invalid encoding option".to_string())
}

fn data_to_bytes(value: &JsonValue) -> Result<Vec<u8>, String> {
    if let Some(s) = value.as_str() {
        return Ok(s.as_bytes().to_vec());
    }

    if let Some(arr) = value.as_array() {
        let mut bytes = Vec::with_capacity(arr.len());
        for (idx, item) in arr.iter().enumerate() {
            let n = item
                .as_u64()
                .ok_or_else(|| format!("Byte at index {idx} must be an integer"))?;
            if n > u8::MAX as u64 {
                return Err(format!("Byte at index {idx} must be in range 0..=255"));
            }
            bytes.push(n as u8);
        }
        return Ok(bytes);
    }

    if let Some(buffer_data) = value
        .get("type")
        .and_then(|v| v.as_str())
        .filter(|t| *t == "Buffer")
        .and_then(|_| value.get("data"))
    {
        return data_to_bytes(buffer_data);
    }

    Err("Data must be a string, byte array, or Buffer JSON object".to_string())
}

fn decode_bytes(bytes: &[u8], encoding: Option<&str>) -> Result<JsonValue, String> {
    match encoding {
        Some("utf8") | Some("utf-8") => Ok(json!(String::from_utf8_lossy(bytes))),
        Some("latin1") | Some("binary") => {
            let s: String = bytes.iter().map(|b| *b as char).collect();
            Ok(json!(s))
        }
        Some("ascii") => {
            let s: String = bytes.iter().map(|b| (*b & 0x7f) as char).collect();
            Ok(json!(s))
        }
        Some(enc) => Err(format!("Unknown encoding: {enc}")),
        None => Ok(json!(bytes)),
    }
}

fn fs_error(op: &str, path: &str, err: io::Error) -> String {
    let code = match err.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::IsADirectory => "EISDIR",
        io::ErrorKind::NotADirectory => "ENOTDIR",
        io::ErrorKind::InvalidInput => "EINVAL",
        _ => "EIO",
    };
    format!("{code}: {op} '{path}': {err}")
}

fn fs_error_two_paths(op: &str, from: &str, to: &str, err: io::Error) -> String {
    let code = match err.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        _ => "EIO",
    };
    format!("{code}: {op} '{from}' -> '{to}': {err}")
}

fn do_write(path: &str, bytes: &[u8], append: bool) -> Result<(), String> {
    if append {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| fs_error("appendFile", path, e))?;
        std::io::Write::write_all(&mut file, bytes).map_err(|e| fs_error("appendFile", path, e))
    } else {
        fs::write(path, bytes).map_err(|e| fs_error("writeFile", path, e))
    }
}

/// Read file synchronously.
fn fs_read_file_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "readFileSync")?;
    security::require_fs_read(path)?;

    let encoding = parse_encoding(args.get(1))?;
    let bytes = fs::read(path).map_err(|e| fs_error("readFileSync", path, e))?;
    decode_bytes(&bytes, encoding.as_deref())
}

/// Write file synchronously.
fn fs_write_file_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "writeFileSync")?;
    let data = args.get(1).ok_or("writeFileSync requires data argument")?;

    security::require_fs_write(path)?;
    let bytes = data_to_bytes(data)?;
    let append = args
        .get(2)
        .and_then(|v| v.get("flag"))
        .and_then(|v| v.as_str())
        .map(|flag| flag.contains('a'))
        .unwrap_or(false);
    do_write(path, &bytes, append)?;
    Ok(json!(null))
}

/// Append file synchronously.
fn fs_append_file_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "appendFileSync")?;
    let data = args.get(1).ok_or("appendFileSync requires data argument")?;

    security::require_fs_write(path)?;
    let bytes = data_to_bytes(data)?;
    do_write(path, &bytes, true)?;
    Ok(json!(null))
}

/// Check if file exists.
fn fs_exists_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "existsSync")?;
    security::require_fs_read(path)?;
    Ok(json!(Path::new(path).exists()))
}

/// Access check.
fn fs_access_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "accessSync")?;
    let mode = args.get(1).and_then(|v| v.as_u64()).unwrap_or(F_OK);

    if mode == F_OK {
        security::require_fs_read(path)?;
    } else {
        if (mode & R_OK) != 0 || (mode & X_OK) != 0 {
            security::require_fs_read(path)?;
        }
        if (mode & W_OK) != 0 {
            security::require_fs_write(path)?;
        }
    }

    fs::metadata(path).map_err(|e| fs_error("accessSync", path, e))?;
    Ok(json!(null))
}

fn fs_stat_for_path(path: &str, op: &str) -> Result<JsonValue, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| fs_error(op, path, e))?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    };
    #[cfg(not(unix))]
    let mode = 0_u32;

    Ok(json!({
        "isFile": metadata.is_file(),
        "isDirectory": metadata.is_dir(),
        "isSymbolicLink": metadata.file_type().is_symlink(),
        "size": metadata.len(),
        "mode": mode,
    }))
}

/// Get file stats.
fn fs_stat_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "statSync")?;
    security::require_fs_read(path)?;
    fs_stat_for_path(path, "statSync")
}

/// Read directory contents.
fn fs_readdir_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "readdirSync")?;
    security::require_fs_read(path)?;

    let mut entries: Vec<String> = Vec::new();
    let reader = fs::read_dir(path).map_err(|e| fs_error("readdirSync", path, e))?;
    for entry in reader {
        let entry = entry.map_err(|e| fs_error("readdirSync", path, e))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "Invalid UTF-8 filename in directory entry".to_string())?;
        entries.push(name);
    }
    Ok(json!(entries))
}

/// Create directory.
fn fs_mkdir_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "mkdirSync")?;
    security::require_fs_write(path)?;

    let recursive = args
        .get(1)
        .and_then(|v| v.get("recursive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if recursive {
        fs::create_dir_all(path).map_err(|e| fs_error("mkdirSync", path, e))?;
    } else {
        fs::create_dir(path).map_err(|e| fs_error("mkdirSync", path, e))?;
    }

    Ok(json!(null))
}

/// Create temporary directory with a prefix.
fn fs_mkdtemp_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let prefix = get_required_string(args, 0, "mkdtempSync")?;

    for _ in 0..128 {
        let seq = MKDTEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let suffix = format!("{:x}{:x}", nanos, seq);
        let candidate = format!("{prefix}{suffix}");

        security::require_fs_write(&candidate)?;

        match fs::create_dir(&candidate) {
            Ok(_) => return Ok(json!(candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(fs_error("mkdtempSync", &candidate, e)),
        }
    }

    Err("EEXIST: mkdtempSync exhausted unique suffix attempts".to_string())
}

/// Remove directory.
fn fs_rmdir_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "rmdirSync")?;
    security::require_fs_write(path)?;

    let recursive = args
        .get(1)
        .and_then(|v| v.get("recursive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if recursive {
        fs::remove_dir_all(path).map_err(|e| fs_error("rmdirSync", path, e))?;
    } else {
        fs::remove_dir(path).map_err(|e| fs_error("rmdirSync", path, e))?;
    }

    Ok(json!(null))
}

/// Remove file or directory.
fn fs_rm_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "rmSync")?;
    security::require_fs_write(path)?;

    let recursive = args
        .get(1)
        .and_then(|v| v.get("recursive"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let force = args
        .get(1)
        .and_then(|v| v.get("force"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_dir() {
                if recursive {
                    fs::remove_dir_all(path).map_err(|e| fs_error("rmSync", path, e))?;
                } else {
                    fs::remove_dir(path).map_err(|e| fs_error("rmSync", path, e))?;
                }
            } else {
                fs::remove_file(path).map_err(|e| fs_error("rmSync", path, e))?;
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && force => {}
        Err(e) => return Err(fs_error("rmSync", path, e)),
    }

    Ok(json!(null))
}

/// Delete file.
fn fs_unlink_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "unlinkSync")?;
    security::require_fs_write(path)?;

    fs::remove_file(path).map_err(|e| fs_error("unlinkSync", path, e))?;
    Ok(json!(null))
}

/// Copy file.
fn fs_copy_file_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let src = get_required_string(args, 0, "copyFileSync")?;
    let dst = get_required_string(args, 1, "copyFileSync")?;

    security::require_fs_read(src)?;
    security::require_fs_write(dst)?;
    fs::copy(src, dst).map_err(|e| fs_error_two_paths("copyFileSync", src, dst, e))?;
    Ok(json!(null))
}

/// Rename/move file.
fn fs_rename_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let old_path = get_required_string(args, 0, "renameSync")?;
    let new_path = get_required_string(args, 1, "renameSync")?;

    security::require_fs_write(old_path)?;
    security::require_fs_write(new_path)?;
    fs::rename(old_path, new_path)
        .map_err(|e| fs_error_two_paths("renameSync", old_path, new_path, e))?;
    Ok(json!(null))
}

/// Resolve real path.
fn fs_realpath_sync(args: &[JsonValue]) -> Result<JsonValue, String> {
    let path = get_required_string(args, 0, "realpathSync")?;
    security::require_fs_read(path)?;

    let canonical = dunce::canonicalize(path).map_err(|e| fs_error("realpathSync", path, e))?;
    Ok(json!(canonical.to_string_lossy()))
}

/// Read file async.
fn fs_read_file_async(
    args: &[JsonValue],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsonValue, String>> + Send>> {
    let path = match get_required_string(args, 0, "readFile") {
        Ok(path) => path.to_string(),
        Err(e) => return Box::pin(async move { Err(e) }),
    };
    let encoding = match parse_encoding(args.get(1)) {
        Ok(encoding) => encoding,
        Err(e) => return Box::pin(async move { Err(e) }),
    };

    if let Err(e) = security::require_fs_read(&path) {
        return Box::pin(async move { Err(e) });
    }

    Box::pin(async move {
        let bytes = fs::read(&path).map_err(|e| fs_error("readFile", &path, e))?;
        decode_bytes(&bytes, encoding.as_deref())
    })
}

/// Write file async.
fn fs_write_file_async(
    args: &[JsonValue],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsonValue, String>> + Send>> {
    let path = match get_required_string(args, 0, "writeFile") {
        Ok(path) => path.to_string(),
        Err(e) => return Box::pin(async move { Err(e) }),
    };
    let data = match args.get(1) {
        Some(data) => data.clone(),
        None => return Box::pin(async { Err("writeFile requires data argument".to_string()) }),
    };

    if let Err(e) = security::require_fs_write(&path) {
        return Box::pin(async move { Err(e) });
    }

    let bytes = match data_to_bytes(&data) {
        Ok(bytes) => bytes,
        Err(e) => return Box::pin(async move { Err(e) }),
    };
    let append = args
        .get(2)
        .and_then(|v| v.get("flag"))
        .and_then(|v| v.as_str())
        .map(|flag| flag.contains('a'))
        .unwrap_or(false);

    Box::pin(async move {
        do_write(&path, &bytes, append)?;
        Ok(json!(null))
    })
}

/// Stat async.
fn fs_stat_async(
    args: &[JsonValue],
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<JsonValue, String>> + Send>> {
    let path = match get_required_string(args, 0, "stat") {
        Ok(path) => path.to_string(),
        Err(e) => return Box::pin(async move { Err(e) }),
    };

    if let Err(e) = security::require_fs_read(&path) {
        return Box::pin(async move { Err(e) });
    }

    Box::pin(async move { fs_stat_for_path(&path, "stat") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_runtime::{CapabilitiesBuilder, CapabilitiesGuard};
    use tempfile::tempdir;

    #[test]
    fn test_fs_read_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.txt");
        let path_str = path.to_string_lossy().to_string();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![dir.path().to_path_buf()])
            .allow_write(vec![dir.path().to_path_buf()])
            .build();
        let _guard = CapabilitiesGuard::new(caps);

        fs_write_file_sync(&[json!(path_str), json!("Hello")]).unwrap();
        let result = fs_read_file_sync(&[json!(path.to_string_lossy()), json!("utf8")]).unwrap();
        assert_eq!(result, json!("Hello"));
    }

    #[test]
    fn test_fs_denied_without_capabilities() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("denied.txt");
        std::fs::write(&path, "hello").unwrap();

        let err = fs_read_file_sync(&[json!(path.to_string_lossy()), json!("utf8")]).unwrap_err();
        assert!(err.contains("PermissionDenied"));
    }

    #[test]
    fn test_append_and_rm() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("append.txt");
        let path_str = path.to_string_lossy().to_string();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![dir.path().to_path_buf()])
            .allow_write(vec![dir.path().to_path_buf()])
            .build();
        let _guard = CapabilitiesGuard::new(caps);

        fs_write_file_sync(&[json!(path_str.clone()), json!("a")]).unwrap();
        fs_append_file_sync(&[json!(path_str.clone()), json!("b")]).unwrap();
        let content = fs_read_file_sync(&[json!(path_str.clone()), json!("utf8")]).unwrap();
        assert_eq!(content, json!("ab"));

        fs_rm_sync(&[json!(path_str), json!({"force": false})]).unwrap();
        assert!(!path.exists());
    }
}
