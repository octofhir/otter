//! node:fs implementation
//!
//! Promise-based file system operations with capability-based security.
//! All operations require appropriate permissions via `Capabilities`.

use otter_engine::Capabilities;
use std::path::Path;
use std::time::UNIX_EPOCH;
use tokio::fs;

/// File system error.
#[derive(Debug)]
pub enum FsError {
    PermissionDenied(String),
    IoError(std::io::Error),
    InvalidUtf8,
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::PermissionDenied(msg) => write!(f, "Permission denied: {}", msg),
            FsError::IoError(e) => write!(f, "IO error: {}", e),
            FsError::InvalidUtf8 => write!(f, "Invalid UTF-8"),
        }
    }
}

impl std::error::Error for FsError {}

impl From<std::io::Error> for FsError {
    fn from(e: std::io::Error) -> Self {
        FsError::IoError(e)
    }
}

/// File statistics.
#[derive(Debug, Clone)]
pub struct Stats {
    pub is_file: bool,
    pub is_directory: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub mtime_ms: u64,
    pub atime_ms: u64,
    pub ctime_ms: u64,
}

/// Read a file as string.
pub async fn read_file(
    caps: &Capabilities,
    path: &str,
    encoding: Option<&str>,
) -> Result<ReadResult, FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_read(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "read access to '{}'. Use --allow-read to grant permission.",
            path
        )));
    }

    let contents = fs::read(&path_buf).await?;

    match encoding {
        Some("utf8") | Some("utf-8") => {
            let text = String::from_utf8(contents).map_err(|_| FsError::InvalidUtf8)?;
            Ok(ReadResult::String(text))
        }
        _ => Ok(ReadResult::Bytes(contents)),
    }
}

/// Result of reading a file.
#[derive(Debug)]
pub enum ReadResult {
    String(String),
    Bytes(Vec<u8>),
}

/// Write data to a file.
pub async fn write_file(caps: &Capabilities, path: &str, data: &[u8]) -> Result<(), FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_write(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "write access to '{}'. Use --allow-write to grant permission.",
            path
        )));
    }

    fs::write(&path_buf, data).await?;
    Ok(())
}

/// Read directory contents.
pub async fn readdir(caps: &Capabilities, path: &str) -> Result<Vec<String>, FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_read(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "read access to '{}'. Use --allow-read to grant permission.",
            path
        )));
    }

    let mut entries = Vec::new();
    let mut dir = fs::read_dir(&path_buf).await?;

    while let Some(entry) = dir.next_entry().await? {
        entries.push(entry.file_name().to_string_lossy().to_string());
    }

    Ok(entries)
}

/// Get file statistics.
pub async fn stat(caps: &Capabilities, path: &str) -> Result<Stats, FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_read(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "read access to '{}'. Use --allow-read to grant permission.",
            path
        )));
    }

    let metadata = fs::metadata(&path_buf).await?;
    let file_type = metadata.file_type();

    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let atime_ms = metadata
        .accessed()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let ctime_ms = metadata
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        metadata.mode()
    };
    #[cfg(not(unix))]
    let mode = 0u32;

    Ok(Stats {
        is_file: file_type.is_file(),
        is_directory: file_type.is_dir(),
        is_symlink: file_type.is_symlink(),
        size: metadata.len(),
        mode,
        mtime_ms,
        atime_ms,
        ctime_ms,
    })
}

/// Create a directory.
pub async fn mkdir(caps: &Capabilities, path: &str, recursive: bool) -> Result<(), FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_write(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "write access to '{}'. Use --allow-write to grant permission.",
            path
        )));
    }

    if recursive {
        fs::create_dir_all(&path_buf).await?;
    } else {
        fs::create_dir(&path_buf).await?;
    }

    Ok(())
}

/// Remove a file or directory.
pub async fn rm(caps: &Capabilities, path: &str, recursive: bool) -> Result<(), FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_write(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "write access to '{}'. Use --allow-write to grant permission.",
            path
        )));
    }

    let metadata = fs::metadata(&path_buf).await?;

    if metadata.is_dir() {
        if recursive {
            fs::remove_dir_all(&path_buf).await?;
        } else {
            fs::remove_dir(&path_buf).await?;
        }
    } else {
        fs::remove_file(&path_buf).await?;
    }

    Ok(())
}

/// Unlink (remove) a file.
pub async fn unlink(caps: &Capabilities, path: &str) -> Result<(), FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_write(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "write access to '{}'. Use --allow-write to grant permission.",
            path
        )));
    }

    fs::remove_file(&path_buf).await?;
    Ok(())
}

/// Check if a path exists.
pub async fn exists(caps: &Capabilities, path: &str) -> Result<bool, FsError> {
    let path_buf = Path::new(path).to_path_buf();

    if !caps.can_read(&path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "read access to '{}'. Use --allow-read to grant permission.",
            path
        )));
    }

    Ok(path_buf.exists())
}

/// Rename/move a file or directory.
pub async fn rename(caps: &Capabilities, old_path: &str, new_path: &str) -> Result<(), FsError> {
    let old_path_buf = Path::new(old_path).to_path_buf();
    let new_path_buf = Path::new(new_path).to_path_buf();

    if !caps.can_read(&old_path_buf) {
        return Err(FsError::PermissionDenied(format!(
            "read access to '{}'. Use --allow-read to grant permission.",
            old_path
        )));
    }

    if !caps.can_write(&old_path_buf) || !caps.can_write(&new_path_buf) {
        return Err(FsError::PermissionDenied(
            "write access for rename. Use --allow-write to grant permission.".to_string(),
        ));
    }

    fs::rename(&old_path_buf, &new_path_buf).await?;
    Ok(())
}

/// Copy a file.
pub async fn copy_file(caps: &Capabilities, src: &str, dest: &str) -> Result<u64, FsError> {
    let src_path = Path::new(src).to_path_buf();
    let dest_path = Path::new(dest).to_path_buf();

    if !caps.can_read(&src_path) {
        return Err(FsError::PermissionDenied(format!(
            "read access to '{}'. Use --allow-read to grant permission.",
            src
        )));
    }

    if !caps.can_write(&dest_path) {
        return Err(FsError::PermissionDenied(format!(
            "write access to '{}'. Use --allow-write to grant permission.",
            dest
        )));
    }

    let bytes = fs::copy(&src_path, &dest_path).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_engine::CapabilitiesBuilder;
    use tempfile::TempDir;

    // Helper to get canonical temp path (handles macOS /var -> /private/var symlink)
    fn canonical_temp_path(temp: &TempDir) -> std::path::PathBuf {
        temp.path()
            .canonicalize()
            .unwrap_or_else(|_| temp.path().to_path_buf())
    }

    #[tokio::test]
    async fn test_read_write_file() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let file_path = temp_path.join("test.txt");

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .allow_write(vec![temp_path.clone()])
            .build();

        // Write
        write_file(&caps, file_path.to_str().unwrap(), b"hello world")
            .await
            .unwrap();

        // Read as bytes
        let result = read_file(&caps, file_path.to_str().unwrap(), None)
            .await
            .unwrap();
        match result {
            ReadResult::Bytes(bytes) => assert_eq!(bytes, b"hello world"),
            _ => panic!("Expected bytes"),
        }

        // Read as string
        let result = read_file(&caps, file_path.to_str().unwrap(), Some("utf8"))
            .await
            .unwrap();
        match result {
            ReadResult::String(s) => assert_eq!(s, "hello world"),
            _ => panic!("Expected string"),
        }
    }

    #[tokio::test]
    async fn test_permission_denied() {
        let caps = Capabilities::none();

        let result = read_file(&caps, "/etc/passwd", None).await;
        assert!(matches!(result, Err(FsError::PermissionDenied(_))));
    }

    #[tokio::test]
    async fn test_readdir() {
        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("file1.txt");
        let file2 = temp.path().join("file2.txt");

        std::fs::write(&file1, "content1").unwrap();
        std::fs::write(&file2, "content2").unwrap();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp.path().to_path_buf()])
            .build();

        let entries = readdir(&caps, temp.path().to_str().unwrap()).await.unwrap();

        assert!(entries.contains(&"file1.txt".to_string()));
        assert!(entries.contains(&"file2.txt".to_string()));
    }

    #[tokio::test]
    async fn test_stat() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp.path().to_path_buf()])
            .build();

        let stats = stat(&caps, file_path.to_str().unwrap()).await.unwrap();
        assert!(stats.is_file);
        assert!(!stats.is_directory);
        assert_eq!(stats.size, 5);
    }

    #[tokio::test]
    async fn test_mkdir_rm() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let dir_path = temp_path.join("subdir");

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .allow_write(vec![temp_path.clone()])
            .build();

        // Create directory
        mkdir(&caps, dir_path.to_str().unwrap(), false)
            .await
            .unwrap();
        assert!(dir_path.exists());

        // Remove directory
        rm(&caps, dir_path.to_str().unwrap(), false).await.unwrap();
        assert!(!dir_path.exists());
    }

    #[tokio::test]
    async fn test_mkdir_recursive() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let deep_path = temp_path.join("a/b/c");

        let caps = CapabilitiesBuilder::new()
            .allow_write(vec![temp_path.clone()])
            .build();

        mkdir(&caps, deep_path.to_str().unwrap(), true)
            .await
            .unwrap();
        assert!(deep_path.exists());
    }

    #[tokio::test]
    async fn test_exists() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let file_path = temp_path.join("test.txt");

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .build();

        assert!(!exists(&caps, file_path.to_str().unwrap()).await.unwrap());

        std::fs::write(&file_path, "hello").unwrap();

        assert!(exists(&caps, file_path.to_str().unwrap()).await.unwrap());
    }

    #[tokio::test]
    async fn test_copy_file() {
        let temp = TempDir::new().unwrap();
        let temp_path = canonical_temp_path(&temp);
        let src = temp_path.join("src.txt");
        let dest = temp_path.join("dest.txt");

        std::fs::write(&src, "hello world").unwrap();

        let caps = CapabilitiesBuilder::new()
            .allow_read(vec![temp_path.clone()])
            .allow_write(vec![temp_path.clone()])
            .build();

        let bytes = copy_file(&caps, src.to_str().unwrap(), dest.to_str().unwrap())
            .await
            .unwrap();

        assert_eq!(bytes, 11);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello world");
    }
}
