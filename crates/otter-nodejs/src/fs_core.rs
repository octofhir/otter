//! Shared filesystem operation core for `node:fs` and `node:fs/promises`.
//!
//! This module keeps I/O, capability checks, and error mapping in one place so
//! sync and async exports can reuse the same operation logic.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const F_OK: u64 = 0;
const R_OK: u64 = 4;
const W_OK: u64 = 2;
const X_OK: u64 = 1;

static MKDTEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static FILE_HANDLE_COUNTER: AtomicU64 = AtomicU64::new(1);
static FILE_HANDLES: LazyLock<Mutex<HashMap<u64, std::fs::File>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Open options for `fs/promises.open()`.
#[derive(Debug, Clone, Copy)]
pub struct FsOpenOptions {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
}

impl FsOpenOptions {
    pub fn from_flag(flag: &str) -> Option<Self> {
        let options = match flag {
            "r" => Self {
                read: true,
                write: false,
                append: false,
                truncate: false,
                create: false,
                create_new: false,
            },
            "r+" => Self {
                read: true,
                write: true,
                append: false,
                truncate: false,
                create: false,
                create_new: false,
            },
            "w" => Self {
                read: false,
                write: true,
                append: false,
                truncate: true,
                create: true,
                create_new: false,
            },
            "w+" => Self {
                read: true,
                write: true,
                append: false,
                truncate: true,
                create: true,
                create_new: false,
            },
            "a" => Self {
                read: false,
                write: true,
                append: true,
                truncate: false,
                create: true,
                create_new: false,
            },
            "a+" => Self {
                read: true,
                write: true,
                append: true,
                truncate: false,
                create: true,
                create_new: false,
            },
            "ax" => Self {
                read: false,
                write: true,
                append: true,
                truncate: false,
                create: true,
                create_new: true,
            },
            "ax+" => Self {
                read: true,
                write: true,
                append: true,
                truncate: false,
                create: true,
                create_new: true,
            },
            "wx" => Self {
                read: false,
                write: true,
                append: false,
                truncate: true,
                create: true,
                create_new: true,
            },
            "wx+" => Self {
                read: true,
                write: true,
                append: false,
                truncate: true,
                create: true,
                create_new: true,
            },
            _ => return None,
        };
        Some(options)
    }

    fn apply(&self, options: &mut std::fs::OpenOptions) {
        options
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .truncate(self.truncate)
            .create(self.create)
            .create_new(self.create_new);
    }

    fn requires_read(&self) -> bool {
        self.read
    }

    fn requires_write(&self) -> bool {
        self.write || self.append || self.truncate || self.create || self.create_new
    }
}

/// Normalized filesystem operation request.
pub enum FsOp {
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        bytes: Vec<u8>,
        append: bool,
    },
    Stat {
        path: String,
        follow_symlinks: bool,
    },
    Readdir {
        path: String,
    },
    Mkdir {
        path: String,
        recursive: bool,
    },
    Mkdtemp {
        prefix: String,
    },
    Rm {
        path: String,
        recursive: bool,
        force: bool,
    },
    Unlink {
        path: String,
    },
    CopyFile {
        src: String,
        dst: String,
    },
    Cp {
        src: String,
        dst: String,
        recursive: bool,
        force: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Open {
        path: String,
        flags: FsOpenOptions,
    },
    CloseHandle {
        handle_id: u64,
    },
    Realpath {
        path: String,
    },
    Access {
        path: String,
        mode: u64,
    },
    Chmod {
        path: String,
        mode: u32,
    },
    Symlink {
        target: String,
        link_path: String,
    },
    Readlink {
        path: String,
    },
}

/// Normalized operation result without JS VM types.
pub enum FsOpResult {
    Bytes(Vec<u8>),
    Unit,
    Metadata(FsMetadata),
    Strings(Vec<String>),
    String(String),
    FileHandle(u64),
}

/// Stable metadata payload used by both sync and async entry points.
#[derive(Debug, Clone)]
pub struct FsMetadata {
    pub size: u64,
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub atime_ms: f64,
    pub mtime_ms: f64,
    pub ctime_ms: f64,
    pub birthtime_ms: f64,
    pub is_file: bool,
    pub is_dir: bool,
    pub is_symlink: bool,
}

/// Node-style fs operation error (code + syscall + path context).
#[derive(Debug, Clone)]
pub struct FsOpError {
    pub code: &'static str,
    pub syscall: &'static str,
    pub path: Option<String>,
    pub dest: Option<String>,
    pub detail: String,
}

impl FsOpError {
    fn from_io(syscall: &'static str, path: &str, err: io::Error) -> Self {
        Self {
            code: io_kind_to_code(err.kind()),
            syscall,
            path: Some(path.to_string()),
            dest: None,
            detail: err.to_string(),
        }
    }

    fn security(syscall: &'static str, path: &str, msg: String) -> Self {
        Self {
            code: "EACCES",
            syscall,
            path: Some(path.to_string()),
            dest: None,
            detail: msg,
        }
    }

    fn from_io_two(syscall: &'static str, from: &str, to: &str, err: io::Error) -> Self {
        Self {
            code: io_kind_to_code(err.kind()),
            syscall,
            path: Some(from.to_string()),
            dest: Some(to.to_string()),
            detail: err.to_string(),
        }
    }

    fn invalid(syscall: &'static str, path: &str, detail: impl Into<String>) -> Self {
        Self {
            code: "EINVAL",
            syscall,
            path: Some(path.to_string()),
            dest: None,
            detail: detail.into(),
        }
    }

    fn unsupported(syscall: &'static str, path: &str, detail: impl Into<String>) -> Self {
        Self {
            code: "ENOSYS",
            syscall,
            path: Some(path.to_string()),
            dest: None,
            detail: detail.into(),
        }
    }

    fn internal(syscall: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code: "EIO",
            syscall,
            path: None,
            dest: None,
            detail: detail.into(),
        }
    }

    fn bad_handle(syscall: &'static str, handle_id: u64) -> Self {
        Self {
            code: "EBADF",
            syscall,
            path: None,
            dest: None,
            detail: format!("Invalid file handle id: {handle_id}"),
        }
    }
}

impl fmt::Display for FsOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.path, &self.dest) {
            (Some(path), Some(dest)) => {
                write!(
                    f,
                    "{}: {} '{}' -> '{}': {}",
                    self.code, self.syscall, path, dest, self.detail
                )
            }
            (Some(path), None) => {
                write!(
                    f,
                    "{}: {} '{}': {}",
                    self.code, self.syscall, path, self.detail
                )
            }
            _ => write!(f, "{}: {}: {}", self.code, self.syscall, self.detail),
        }
    }
}

impl std::error::Error for FsOpError {}

fn next_mkdtemp_path(prefix: &str) -> String {
    let seq = MKDTEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = format!("{:x}{:x}", nanos, seq);
    format!("{prefix}{suffix}")
}

fn create_mkdtemp_dir(prefix: &str) -> Result<String, FsOpError> {
    for _ in 0..128 {
        let candidate = next_mkdtemp_path(prefix);
        match std::fs::create_dir(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(FsOpError::from_io("mkdtemp", &candidate, e)),
        }
    }
    Err(FsOpError::invalid(
        "mkdtemp",
        prefix,
        "Exhausted unique suffix attempts",
    ))
}

fn copy_dir_recursive_sync(src: &Path, dst: &Path, force: bool) -> io::Result<()> {
    if !src.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a directory",
        ));
    }

    if dst.exists() {
        if !dst.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination exists and is not a directory",
            ));
        }
    } else {
        std::fs::create_dir_all(dst)?;
    }

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive_sync(&src_path, &dst_path, force)?;
            continue;
        }

        if dst_path.exists() && force {
            if dst_path.is_dir() {
                std::fs::remove_dir_all(&dst_path)?;
            } else {
                std::fs::remove_file(&dst_path)?;
            }
        }

        if dst_path.exists() && !force {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination entry already exists",
            ));
        }

        std::fs::copy(&src_path, &dst_path)?;
    }

    Ok(())
}

fn cp_sync(src: &str, dst: &str, recursive: bool, force: bool) -> Result<(), FsOpError> {
    let src_path = Path::new(src);
    let metadata =
        std::fs::symlink_metadata(src_path).map_err(|e| FsOpError::from_io("cp", src, e))?;
    if metadata.file_type().is_dir() {
        if !recursive {
            return Err(FsOpError::invalid(
                "cp",
                src,
                "Source is a directory, set recursive option to true",
            ));
        }
        copy_dir_recursive_sync(src_path, Path::new(dst), force)
            .map_err(|e| FsOpError::from_io_two("cp", src, dst, e))?;
    } else {
        if Path::new(dst).exists() && !force {
            return Err(FsOpError::from_io_two(
                "cp",
                src,
                dst,
                io::Error::new(io::ErrorKind::AlreadyExists, "destination already exists"),
            ));
        }
        std::fs::copy(src, dst).map_err(|e| FsOpError::from_io_two("cp", src, dst, e))?;
    }
    Ok(())
}

fn store_file_handle(file: std::fs::File) -> Result<u64, FsOpError> {
    let handle_id = FILE_HANDLE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut guard = FILE_HANDLES
        .lock()
        .map_err(|_| FsOpError::internal("open", "File handle registry is poisoned"))?;
    guard.insert(handle_id, file);
    Ok(handle_id)
}

fn close_file_handle(handle_id: u64) -> Result<(), FsOpError> {
    let mut guard = FILE_HANDLES
        .lock()
        .map_err(|_| FsOpError::internal("close", "File handle registry is poisoned"))?;
    if guard.remove(&handle_id).is_none() {
        return Err(FsOpError::bad_handle("close", handle_id));
    }
    Ok(())
}

/// Execute an fs operation synchronously.
pub fn execute_sync(op: FsOp) -> Result<FsOpResult, FsOpError> {
    match op {
        FsOp::ReadFile { path } => {
            crate::security::require_fs_read(&path)
                .map_err(|e| FsOpError::security("readFile", &path, e))?;
            let bytes =
                std::fs::read(&path).map_err(|e| FsOpError::from_io("readFile", &path, e))?;
            Ok(FsOpResult::Bytes(bytes))
        }
        FsOp::WriteFile {
            path,
            bytes,
            append,
        } => {
            crate::security::require_fs_write(&path)
                .map_err(|e| FsOpError::security("writeFile", &path, e))?;

            if append {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| FsOpError::from_io("writeFile", &path, e))?;
                io::Write::write_all(&mut file, &bytes)
                    .map_err(|e| FsOpError::from_io("writeFile", &path, e))?;
            } else {
                std::fs::write(&path, &bytes)
                    .map_err(|e| FsOpError::from_io("writeFile", &path, e))?;
            }

            Ok(FsOpResult::Unit)
        }
        FsOp::Stat {
            path,
            follow_symlinks,
        } => {
            let syscall = if follow_symlinks { "stat" } else { "lstat" };
            crate::security::require_fs_read(&path)
                .map_err(|e| FsOpError::security(syscall, &path, e))?;

            let metadata = if follow_symlinks {
                std::fs::metadata(&path)
            } else {
                std::fs::symlink_metadata(&path)
            }
            .map_err(|e| FsOpError::from_io(syscall, &path, e))?;

            Ok(FsOpResult::Metadata(metadata_to_core(&metadata)))
        }
        FsOp::Readdir { path } => {
            crate::security::require_fs_read(&path)
                .map_err(|e| FsOpError::security("readdir", &path, e))?;

            let reader =
                std::fs::read_dir(&path).map_err(|e| FsOpError::from_io("readdir", &path, e))?;
            let mut names = Vec::new();
            for entry in reader {
                let entry = entry.map_err(|e| FsOpError::from_io("readdir", &path, e))?;
                let name = entry.file_name().into_string().map_err(|_| {
                    FsOpError::invalid(
                        "readdir",
                        &path,
                        "Invalid UTF-8 filename in directory entry",
                    )
                })?;
                names.push(name);
            }
            Ok(FsOpResult::Strings(names))
        }
        FsOp::Mkdir { path, recursive } => {
            crate::security::require_fs_write(&path)
                .map_err(|e| FsOpError::security("mkdir", &path, e))?;

            if recursive {
                std::fs::create_dir_all(&path)
                    .map_err(|e| FsOpError::from_io("mkdir", &path, e))?;
            } else {
                std::fs::create_dir(&path).map_err(|e| FsOpError::from_io("mkdir", &path, e))?;
            }

            Ok(FsOpResult::Unit)
        }
        FsOp::Mkdtemp { prefix } => {
            crate::security::require_fs_write(&prefix)
                .map_err(|e| FsOpError::security("mkdtemp", &prefix, e))?;
            let path = create_mkdtemp_dir(&prefix)?;
            Ok(FsOpResult::String(path))
        }
        FsOp::Rm {
            path,
            recursive,
            force,
        } => {
            crate::security::require_fs_write(&path)
                .map_err(|e| FsOpError::security("rm", &path, e))?;

            match std::fs::symlink_metadata(&path) {
                Ok(meta) => {
                    if meta.file_type().is_dir() {
                        if recursive {
                            std::fs::remove_dir_all(&path)
                                .map_err(|e| FsOpError::from_io("rm", &path, e))?;
                        } else {
                            std::fs::remove_dir(&path)
                                .map_err(|e| FsOpError::from_io("rm", &path, e))?;
                        }
                    } else {
                        std::fs::remove_file(&path)
                            .map_err(|e| FsOpError::from_io("rm", &path, e))?;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && force => {}
                Err(e) => return Err(FsOpError::from_io("rm", &path, e)),
            }

            Ok(FsOpResult::Unit)
        }
        FsOp::Unlink { path } => {
            crate::security::require_fs_write(&path)
                .map_err(|e| FsOpError::security("unlink", &path, e))?;
            std::fs::remove_file(&path).map_err(|e| FsOpError::from_io("unlink", &path, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::CopyFile { src, dst } => {
            crate::security::require_fs_read(&src)
                .map_err(|e| FsOpError::security("copyFile", &src, e))?;
            crate::security::require_fs_write(&dst)
                .map_err(|e| FsOpError::security("copyFile", &dst, e))?;
            std::fs::copy(&src, &dst)
                .map_err(|e| FsOpError::from_io_two("copyFile", &src, &dst, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Cp {
            src,
            dst,
            recursive,
            force,
        } => {
            crate::security::require_fs_read(&src)
                .map_err(|e| FsOpError::security("cp", &src, e))?;
            crate::security::require_fs_write(&dst)
                .map_err(|e| FsOpError::security("cp", &dst, e))?;
            cp_sync(&src, &dst, recursive, force)?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Rename { from, to } => {
            crate::security::require_fs_write(&from)
                .map_err(|e| FsOpError::security("rename", &from, e))?;
            crate::security::require_fs_write(&to)
                .map_err(|e| FsOpError::security("rename", &to, e))?;
            std::fs::rename(&from, &to)
                .map_err(|e| FsOpError::from_io_two("rename", &from, &to, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Open { path, flags } => {
            if flags.requires_read() {
                crate::security::require_fs_read(&path)
                    .map_err(|e| FsOpError::security("open", &path, e))?;
            }
            if flags.requires_write() {
                crate::security::require_fs_write(&path)
                    .map_err(|e| FsOpError::security("open", &path, e))?;
            }

            let mut options = std::fs::OpenOptions::new();
            flags.apply(&mut options);
            let file = options
                .open(&path)
                .map_err(|e| FsOpError::from_io("open", &path, e))?;
            let handle_id = store_file_handle(file)?;
            Ok(FsOpResult::FileHandle(handle_id))
        }
        FsOp::CloseHandle { handle_id } => {
            close_file_handle(handle_id)?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Realpath { path } => {
            crate::security::require_fs_read(&path)
                .map_err(|e| FsOpError::security("realpath", &path, e))?;
            let canonical =
                dunce::canonicalize(&path).map_err(|e| FsOpError::from_io("realpath", &path, e))?;
            Ok(FsOpResult::String(canonical.to_string_lossy().into_owned()))
        }
        FsOp::Access { path, mode } => {
            check_access_capabilities(&path, mode)?;
            std::fs::metadata(&path).map_err(|e| FsOpError::from_io("access", &path, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Chmod { path, mode } => {
            crate::security::require_fs_write(&path)
                .map_err(|e| FsOpError::security("chmod", &path, e))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| FsOpError::from_io("chmod", &path, e))?;
            }
            #[cfg(not(unix))]
            {
                let _ = (path, mode);
            }
            Ok(FsOpResult::Unit)
        }
        FsOp::Symlink { target, link_path } => {
            crate::security::require_fs_write(&link_path)
                .map_err(|e| FsOpError::security("symlink", &link_path, e))?;
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target, &link_path)
                    .map_err(|e| FsOpError::from_io_two("symlink", &target, &link_path, e))?;
                Ok(FsOpResult::Unit)
            }
            #[cfg(not(unix))]
            {
                let _ = target;
                Err(FsOpError::unsupported(
                    "symlink",
                    &link_path,
                    "symlink is not supported on this platform",
                ))
            }
        }
        FsOp::Readlink { path } => {
            crate::security::require_fs_read(&path)
                .map_err(|e| FsOpError::security("readlink", &path, e))?;
            let target =
                std::fs::read_link(&path).map_err(|e| FsOpError::from_io("readlink", &path, e))?;
            Ok(FsOpResult::String(target.to_string_lossy().into_owned()))
        }
    }
}

/// Validate filesystem capabilities for an operation.
///
/// This check must run on the VM thread before async operations are moved into
/// background tasks, because capability context is thread-local.
pub fn precheck_capabilities(op: &FsOp) -> Result<(), FsOpError> {
    match op {
        FsOp::ReadFile { path } => crate::security::require_fs_read(path)
            .map_err(|e| FsOpError::security("readFile", path, e)),
        FsOp::WriteFile { path, .. } => crate::security::require_fs_write(path)
            .map_err(|e| FsOpError::security("writeFile", path, e)),
        FsOp::Stat {
            path,
            follow_symlinks,
        } => {
            let syscall = if *follow_symlinks { "stat" } else { "lstat" };
            crate::security::require_fs_read(path)
                .map_err(|e| FsOpError::security(syscall, path, e))
        }
        FsOp::Readdir { path } => crate::security::require_fs_read(path)
            .map_err(|e| FsOpError::security("readdir", path, e)),
        FsOp::Mkdir { path, .. } => crate::security::require_fs_write(path)
            .map_err(|e| FsOpError::security("mkdir", path, e)),
        FsOp::Mkdtemp { prefix } => crate::security::require_fs_write(prefix)
            .map_err(|e| FsOpError::security("mkdtemp", prefix, e)),
        FsOp::Rm { path, .. } => {
            crate::security::require_fs_write(path).map_err(|e| FsOpError::security("rm", path, e))
        }
        FsOp::Unlink { path } => crate::security::require_fs_write(path)
            .map_err(|e| FsOpError::security("unlink", path, e)),
        FsOp::CopyFile { src, dst } => {
            crate::security::require_fs_read(src)
                .map_err(|e| FsOpError::security("copyFile", src, e))?;
            crate::security::require_fs_write(dst)
                .map_err(|e| FsOpError::security("copyFile", dst, e))
        }
        FsOp::Cp { src, dst, .. } => {
            crate::security::require_fs_read(src).map_err(|e| FsOpError::security("cp", src, e))?;
            crate::security::require_fs_write(dst).map_err(|e| FsOpError::security("cp", dst, e))
        }
        FsOp::Rename { from, to } => {
            crate::security::require_fs_write(from)
                .map_err(|e| FsOpError::security("rename", from, e))?;
            crate::security::require_fs_write(to).map_err(|e| FsOpError::security("rename", to, e))
        }
        FsOp::Open { path, flags } => {
            if flags.requires_read() {
                crate::security::require_fs_read(path)
                    .map_err(|e| FsOpError::security("open", path, e))?;
            }
            if flags.requires_write() {
                crate::security::require_fs_write(path)
                    .map_err(|e| FsOpError::security("open", path, e))?;
            }
            Ok(())
        }
        FsOp::CloseHandle { .. } => Ok(()),
        FsOp::Realpath { path } => crate::security::require_fs_read(path)
            .map_err(|e| FsOpError::security("realpath", path, e)),
        FsOp::Access { path, mode } => check_access_capabilities(path, *mode),
        FsOp::Chmod { path, .. } => crate::security::require_fs_write(path)
            .map_err(|e| FsOpError::security("chmod", path, e)),
        FsOp::Symlink { link_path, .. } => crate::security::require_fs_write(link_path)
            .map_err(|e| FsOpError::security("symlink", link_path, e)),
        FsOp::Readlink { path } => crate::security::require_fs_read(path)
            .map_err(|e| FsOpError::security("readlink", path, e)),
    }
}

/// Execute an fs operation asynchronously.
pub async fn execute_async(op: FsOp) -> Result<FsOpResult, FsOpError> {
    precheck_capabilities(&op)?;
    execute_async_unchecked(op).await
}

/// Execute an fs operation asynchronously without capability checks.
///
/// Use only after a successful `precheck_capabilities`.
pub async fn execute_async_unchecked(op: FsOp) -> Result<FsOpResult, FsOpError> {
    match op {
        FsOp::ReadFile { path } => {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| FsOpError::from_io("readFile", &path, e))?;
            Ok(FsOpResult::Bytes(bytes))
        }
        FsOp::WriteFile {
            path,
            bytes,
            append,
        } => {
            if append {
                use tokio::io::AsyncWriteExt;
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                    .map_err(|e| FsOpError::from_io("writeFile", &path, e))?;
                file.write_all(&bytes)
                    .await
                    .map_err(|e| FsOpError::from_io("writeFile", &path, e))?;
            } else {
                tokio::fs::write(&path, &bytes)
                    .await
                    .map_err(|e| FsOpError::from_io("writeFile", &path, e))?;
            }

            Ok(FsOpResult::Unit)
        }
        FsOp::Stat {
            path,
            follow_symlinks,
        } => {
            let syscall = if follow_symlinks { "stat" } else { "lstat" };
            let metadata = if follow_symlinks {
                tokio::fs::metadata(&path).await
            } else {
                tokio::fs::symlink_metadata(&path).await
            }
            .map_err(|e| FsOpError::from_io(syscall, &path, e))?;

            Ok(FsOpResult::Metadata(metadata_to_core(&metadata)))
        }
        FsOp::Readdir { path } => {
            let mut reader = tokio::fs::read_dir(&path)
                .await
                .map_err(|e| FsOpError::from_io("readdir", &path, e))?;
            let mut names = Vec::new();
            while let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|e| FsOpError::from_io("readdir", &path, e))?
            {
                let name = entry.file_name().into_string().map_err(|_| {
                    FsOpError::invalid(
                        "readdir",
                        &path,
                        "Invalid UTF-8 filename in directory entry",
                    )
                })?;
                names.push(name);
            }
            Ok(FsOpResult::Strings(names))
        }
        FsOp::Mkdir { path, recursive } => {
            if recursive {
                tokio::fs::create_dir_all(&path)
                    .await
                    .map_err(|e| FsOpError::from_io("mkdir", &path, e))?;
            } else {
                tokio::fs::create_dir(&path)
                    .await
                    .map_err(|e| FsOpError::from_io("mkdir", &path, e))?;
            }
            Ok(FsOpResult::Unit)
        }
        FsOp::Mkdtemp { prefix } => {
            let prefix_for_worker = prefix.clone();
            let path = tokio::task::spawn_blocking(move || create_mkdtemp_dir(&prefix_for_worker))
                .await
                .map_err(|e| FsOpError::internal("mkdtemp", e.to_string()))??;
            Ok(FsOpResult::String(path))
        }
        FsOp::Rm {
            path,
            recursive,
            force,
        } => {
            match tokio::fs::symlink_metadata(&path).await {
                Ok(meta) => {
                    if meta.file_type().is_dir() {
                        if recursive {
                            tokio::fs::remove_dir_all(&path)
                                .await
                                .map_err(|e| FsOpError::from_io("rm", &path, e))?;
                        } else {
                            tokio::fs::remove_dir(&path)
                                .await
                                .map_err(|e| FsOpError::from_io("rm", &path, e))?;
                        }
                    } else {
                        tokio::fs::remove_file(&path)
                            .await
                            .map_err(|e| FsOpError::from_io("rm", &path, e))?;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && force => {}
                Err(e) => return Err(FsOpError::from_io("rm", &path, e)),
            }

            Ok(FsOpResult::Unit)
        }
        FsOp::Unlink { path } => {
            tokio::fs::remove_file(&path)
                .await
                .map_err(|e| FsOpError::from_io("unlink", &path, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::CopyFile { src, dst } => {
            tokio::fs::copy(&src, &dst)
                .await
                .map_err(|e| FsOpError::from_io_two("copyFile", &src, &dst, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Cp {
            src,
            dst,
            recursive,
            force,
        } => {
            let src_for_worker = src.clone();
            let dst_for_worker = dst.clone();
            tokio::task::spawn_blocking(move || {
                cp_sync(&src_for_worker, &dst_for_worker, recursive, force)
            })
            .await
            .map_err(|e| FsOpError::internal("cp", e.to_string()))??;
            Ok(FsOpResult::Unit)
        }
        FsOp::Rename { from, to } => {
            tokio::fs::rename(&from, &to)
                .await
                .map_err(|e| FsOpError::from_io_two("rename", &from, &to, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Open { path, flags } => {
            let path_for_worker = path.clone();
            let file = tokio::task::spawn_blocking(move || {
                let mut options = std::fs::OpenOptions::new();
                flags.apply(&mut options);
                options
                    .open(&path_for_worker)
                    .map_err(|e| FsOpError::from_io("open", &path_for_worker, e))
            })
            .await
            .map_err(|e| FsOpError::internal("open", e.to_string()))??;
            let handle_id = store_file_handle(file)?;
            Ok(FsOpResult::FileHandle(handle_id))
        }
        FsOp::CloseHandle { handle_id } => {
            close_file_handle(handle_id)?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Realpath { path } => {
            let canonical = tokio::fs::canonicalize(&path)
                .await
                .map_err(|e| FsOpError::from_io("realpath", &path, e))?;
            Ok(FsOpResult::String(canonical.to_string_lossy().into_owned()))
        }
        FsOp::Access { path, mode: _ } => {
            tokio::fs::metadata(&path)
                .await
                .map_err(|e| FsOpError::from_io("access", &path, e))?;
            Ok(FsOpResult::Unit)
        }
        FsOp::Chmod { path, mode } => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .await
                    .map_err(|e| FsOpError::from_io("chmod", &path, e))?;
            }
            #[cfg(not(unix))]
            {
                let _ = (path, mode);
            }
            Ok(FsOpResult::Unit)
        }
        FsOp::Symlink { target, link_path } => {
            #[cfg(unix)]
            {
                tokio::fs::symlink(&target, &link_path)
                    .await
                    .map_err(|e| FsOpError::from_io_two("symlink", &target, &link_path, e))?;
                Ok(FsOpResult::Unit)
            }
            #[cfg(not(unix))]
            {
                let _ = target;
                Err(FsOpError::unsupported(
                    "symlink",
                    &link_path,
                    "symlink is not supported on this platform",
                ))
            }
        }
        FsOp::Readlink { path } => {
            let target = tokio::fs::read_link(&path)
                .await
                .map_err(|e| FsOpError::from_io("readlink", &path, e))?;
            Ok(FsOpResult::String(target.to_string_lossy().into_owned()))
        }
    }
}

#[inline]
fn io_kind_to_code(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::IsADirectory => "EISDIR",
        io::ErrorKind::NotADirectory => "ENOTDIR",
        io::ErrorKind::InvalidInput => "EINVAL",
        _ => "EIO",
    }
}

fn check_access_capabilities(path: &str, mode: u64) -> Result<(), FsOpError> {
    if mode == F_OK {
        crate::security::require_fs_read(path)
            .map_err(|e| FsOpError::security("access", path, e))?;
    } else {
        if (mode & R_OK) != 0 || (mode & X_OK) != 0 {
            crate::security::require_fs_read(path)
                .map_err(|e| FsOpError::security("access", path, e))?;
        }
        if (mode & W_OK) != 0 {
            crate::security::require_fs_write(path)
                .map_err(|e| FsOpError::security("access", path, e))?;
        }
    }
    Ok(())
}

fn metadata_to_core(metadata: &std::fs::Metadata) -> FsMetadata {
    #[cfg(unix)]
    let (mode, dev, ino, nlink, uid, gid) = {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.mode(),
            metadata.dev(),
            metadata.ino(),
            metadata.nlink(),
            metadata.uid(),
            metadata.gid(),
        )
    };
    #[cfg(not(unix))]
    let (mode, dev, ino, nlink, uid, gid) = (0_u32, 0_u64, 0_u64, 0_u64, 0_u32, 0_u32);

    let atime_ms = time_ms(metadata.accessed());
    let mtime_ms = time_ms(metadata.modified());
    let ctime_ms = mtime_ms;
    let birthtime_ms = time_ms(metadata.created());

    FsMetadata {
        size: metadata.len(),
        mode,
        dev,
        ino,
        nlink,
        uid,
        gid,
        atime_ms,
        mtime_ms,
        ctime_ms,
        birthtime_ms,
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
    }
}

fn time_ms(time: io::Result<SystemTime>) -> f64 {
    time.ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_has_code_and_syscall() {
        let err = FsOpError::from_io(
            "readFile",
            "/tmp/nope",
            io::Error::new(io::ErrorKind::NotFound, "not found"),
        );
        let msg = err.to_string();
        assert!(msg.contains("ENOENT"));
        assert!(msg.contains("readFile"));
    }

    #[test]
    fn error_display_for_two_paths() {
        let err = FsOpError::from_io_two(
            "rename",
            "a",
            "b",
            io::Error::new(io::ErrorKind::AlreadyExists, "exists"),
        );
        let msg = err.to_string();
        assert!(msg.contains("'a' -> 'b'"));
    }
}
