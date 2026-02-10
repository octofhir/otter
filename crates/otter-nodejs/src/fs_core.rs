//! Shared filesystem operation core for `node:fs` and `node:fs/promises`.
//!
//! This module keeps I/O, capability checks, and error mapping in one place so
//! sync and async exports can reuse the same operation logic.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
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
static DIR_HANDLE_COUNTER: AtomicU64 = AtomicU64::new(1);
static DIR_HANDLES: LazyLock<Mutex<HashMap<u64, DirHandleState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct DirHandleState {
    path: String,
    reader: std::fs::ReadDir,
}

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

/// Options for `fs.cp`/`fs.cpSync`.
#[derive(Debug, Clone, Copy)]
pub struct FsCpOptions {
    pub recursive: bool,
    pub force: bool,
    pub error_on_exist: bool,
    pub dereference: bool,
    pub preserve_timestamps: bool,
    pub verbatim_symlinks: bool,
    pub mode: u32,
}

impl Default for FsCpOptions {
    fn default() -> Self {
        Self {
            recursive: false,
            force: true,
            error_on_exist: false,
            dereference: false,
            preserve_timestamps: false,
            verbatim_symlinks: false,
            mode: 0,
        }
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
        with_file_types: bool,
    },
    Opendir {
        path: String,
    },
    ReadDirHandle {
        handle_id: u64,
    },
    CloseDirHandle {
        handle_id: u64,
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
        options: FsCpOptions,
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
    ReadHandle {
        handle_id: u64,
        length: usize,
        position: Option<u64>,
    },
    WriteHandle {
        handle_id: u64,
        bytes: Vec<u8>,
        position: Option<u64>,
    },
    ReadFileHandle {
        handle_id: u64,
    },
    WriteFileHandle {
        handle_id: u64,
        bytes: Vec<u8>,
    },
    StatHandle {
        handle_id: u64,
    },
    TruncateHandle {
        handle_id: u64,
        len: u64,
    },
    SyncHandle {
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
    DirEntries(Vec<FsDirEntry>),
    String(String),
    FileHandle(u64),
    DirHandle { handle_id: u64, path: String },
    DirEntry(Option<FsDirEntry>),
    Count(usize),
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

/// Normalized directory entry payload for `readdir({ withFileTypes: true })`.
#[derive(Debug, Clone)]
pub struct FsDirEntry {
    pub name: String,
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

fn prepare_cp_destination(dst: &Path, options: FsCpOptions) -> io::Result<bool> {
    match std::fs::symlink_metadata(dst) {
        Ok(meta) => {
            if (options.mode & 0x1) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination already exists",
                ));
            }

            if !options.force {
                if options.error_on_exist {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "destination already exists",
                    ));
                }
                return Ok(false);
            }

            if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    "destination is a directory",
                ));
            }

            std::fs::remove_file(dst)?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn create_symlink_for_cp(_src: &Path, target: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(windows)]
fn create_symlink_for_cp(src: &Path, target: &Path, dst: &Path) -> io::Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let points_to_dir = std::fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false);
    if points_to_dir {
        symlink_dir(target, dst)
    } else {
        symlink_file(target, dst)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink_for_cp(_src: &Path, _target: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlink copy is not supported on this platform",
    ))
}

fn copy_timestamps_if_needed(src: &Path, dst: &Path, options: FsCpOptions) -> io::Result<()> {
    if !options.preserve_timestamps {
        return Ok(());
    }

    let metadata = std::fs::metadata(src)?;
    let atime = filetime::FileTime::from_last_access_time(&metadata);
    let mtime = filetime::FileTime::from_last_modification_time(&metadata);
    filetime::set_file_times(dst, atime, mtime)
}

fn copy_file_sync(src: &Path, dst: &Path, options: FsCpOptions) -> io::Result<()> {
    if !prepare_cp_destination(dst, options)? {
        return Ok(());
    }
    std::fs::copy(src, dst)?;
    copy_timestamps_if_needed(src, dst, options)?;
    Ok(())
}

fn copy_symlink_sync(src: &Path, dst: &Path, options: FsCpOptions) -> io::Result<()> {
    if !prepare_cp_destination(dst, options)? {
        return Ok(());
    }
    let target = std::fs::read_link(src)?;
    let _ = options.verbatim_symlinks;
    create_symlink_for_cp(src, &target, dst)
}

fn copy_dir_recursive_sync(src: &Path, dst: &Path, options: FsCpOptions) -> io::Result<()> {
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
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        copy_path_sync(&src_path, &dst_path, options)?;
    }

    copy_timestamps_if_needed(src, dst, options)?;
    Ok(())
}

fn copy_path_sync(src: &Path, dst: &Path, options: FsCpOptions) -> io::Result<()> {
    let src_meta = if options.dereference {
        std::fs::metadata(src)?
    } else {
        std::fs::symlink_metadata(src)?
    };

    if src_meta.file_type().is_symlink() && !options.dereference {
        return copy_symlink_sync(src, dst, options);
    }

    if src_meta.is_dir() {
        if !options.recursive {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Source is a directory, set recursive option to true",
            ));
        }
        return copy_dir_recursive_sync(src, dst, options);
    }

    copy_file_sync(src, dst, options)
}

fn cp_sync(src: &str, dst: &str, options: FsCpOptions) -> Result<(), FsOpError> {
    let src_path = Path::new(src);
    let dst_path = Path::new(dst);
    copy_path_sync(src_path, dst_path, options)
        .map_err(|e| FsOpError::from_io_two("cp", src, dst, e))?;
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

fn with_file_handle_mut<T>(
    handle_id: u64,
    syscall: &'static str,
    f: impl FnOnce(&mut std::fs::File) -> Result<T, FsOpError>,
) -> Result<T, FsOpError> {
    let mut guard = FILE_HANDLES
        .lock()
        .map_err(|_| FsOpError::internal(syscall, "File handle registry is poisoned"))?;
    let file = guard
        .get_mut(&handle_id)
        .ok_or_else(|| FsOpError::bad_handle(syscall, handle_id))?;
    f(file)
}

fn read_handle_sync(
    handle_id: u64,
    length: usize,
    position: Option<u64>,
) -> Result<Vec<u8>, FsOpError> {
    with_file_handle_mut(handle_id, "read", |file| {
        let previous_position = if position.is_some() {
            Some(
                file.stream_position()
                    .map_err(|e| FsOpError::from_io("read", "<handle>", e))?,
            )
        } else {
            None
        };

        if let Some(pos) = position {
            file.seek(SeekFrom::Start(pos))
                .map_err(|e| FsOpError::from_io("read", "<handle>", e))?;
        }

        let mut buf = vec![0_u8; length];
        let read = file
            .read(&mut buf)
            .map_err(|e| FsOpError::from_io("read", "<handle>", e))?;
        buf.truncate(read);

        if let Some(prev) = previous_position {
            file.seek(SeekFrom::Start(prev))
                .map_err(|e| FsOpError::from_io("read", "<handle>", e))?;
        }

        Ok(buf)
    })
}

fn write_handle_sync(
    handle_id: u64,
    bytes: &[u8],
    position: Option<u64>,
) -> Result<usize, FsOpError> {
    with_file_handle_mut(handle_id, "write", |file| {
        let previous_position = if position.is_some() {
            Some(
                file.stream_position()
                    .map_err(|e| FsOpError::from_io("write", "<handle>", e))?,
            )
        } else {
            None
        };

        if let Some(pos) = position {
            file.seek(SeekFrom::Start(pos))
                .map_err(|e| FsOpError::from_io("write", "<handle>", e))?;
        }

        let written = file
            .write(bytes)
            .map_err(|e| FsOpError::from_io("write", "<handle>", e))?;

        if let Some(prev) = previous_position {
            file.seek(SeekFrom::Start(prev))
                .map_err(|e| FsOpError::from_io("write", "<handle>", e))?;
        }

        Ok(written)
    })
}

fn read_file_handle_sync(handle_id: u64) -> Result<Vec<u8>, FsOpError> {
    with_file_handle_mut(handle_id, "readFile", |file| {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| FsOpError::from_io("readFile", "<handle>", e))?;
        Ok(bytes)
    })
}

fn write_file_handle_sync(handle_id: u64, bytes: &[u8]) -> Result<(), FsOpError> {
    with_file_handle_mut(handle_id, "writeFile", |file| {
        file.write_all(bytes)
            .map_err(|e| FsOpError::from_io("writeFile", "<handle>", e))?;
        Ok(())
    })
}

fn stat_handle_sync(handle_id: u64) -> Result<FsMetadata, FsOpError> {
    with_file_handle_mut(handle_id, "stat", |file| {
        let metadata = file
            .metadata()
            .map_err(|e| FsOpError::from_io("stat", "<handle>", e))?;
        Ok(metadata_to_core(&metadata))
    })
}

fn truncate_handle_sync(handle_id: u64, len: u64) -> Result<(), FsOpError> {
    with_file_handle_mut(handle_id, "truncate", |file| {
        file.set_len(len)
            .map_err(|e| FsOpError::from_io("truncate", "<handle>", e))?;
        Ok(())
    })
}

fn sync_handle_sync(handle_id: u64) -> Result<(), FsOpError> {
    with_file_handle_mut(handle_id, "sync", |file| {
        file.sync_all()
            .map_err(|e| FsOpError::from_io("sync", "<handle>", e))?;
        Ok(())
    })
}

fn store_dir_handle(path: &str, reader: std::fs::ReadDir) -> Result<u64, FsOpError> {
    let handle_id = DIR_HANDLE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut guard = DIR_HANDLES
        .lock()
        .map_err(|_| FsOpError::internal("opendir", "Directory handle registry is poisoned"))?;
    guard.insert(
        handle_id,
        DirHandleState {
            path: path.to_string(),
            reader,
        },
    );
    Ok(handle_id)
}

fn close_dir_handle(handle_id: u64) -> Result<(), FsOpError> {
    let mut guard = DIR_HANDLES
        .lock()
        .map_err(|_| FsOpError::internal("close", "Directory handle registry is poisoned"))?;
    if guard.remove(&handle_id).is_none() {
        return Err(FsOpError::bad_handle("close", handle_id));
    }
    Ok(())
}

fn read_dir_handle_next_sync(handle_id: u64) -> Result<Option<FsDirEntry>, FsOpError> {
    let mut guard = DIR_HANDLES
        .lock()
        .map_err(|_| FsOpError::internal("read", "Directory handle registry is poisoned"))?;
    let state = guard
        .get_mut(&handle_id)
        .ok_or_else(|| FsOpError::bad_handle("read", handle_id))?;

    match state.reader.next() {
        Some(Ok(entry)) => {
            let name = entry.file_name().into_string().map_err(|_| {
                FsOpError::invalid(
                    "read",
                    &state.path,
                    "Invalid UTF-8 filename in directory entry",
                )
            })?;
            let file_type = entry
                .file_type()
                .map_err(|e| FsOpError::from_io("read", &state.path, e))?;
            Ok(Some(FsDirEntry {
                name,
                is_file: file_type.is_file(),
                is_dir: file_type.is_dir(),
                is_symlink: file_type.is_symlink(),
            }))
        }
        Some(Err(e)) => Err(FsOpError::from_io("read", &state.path, e)),
        None => Ok(None),
    }
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
        FsOp::Readdir {
            path,
            with_file_types,
        } => {
            crate::security::require_fs_read(&path)
                .map_err(|e| FsOpError::security("readdir", &path, e))?;

            let reader =
                std::fs::read_dir(&path).map_err(|e| FsOpError::from_io("readdir", &path, e))?;
            let mut names = Vec::new();
            let mut entries = Vec::new();
            for entry in reader {
                let entry = entry.map_err(|e| FsOpError::from_io("readdir", &path, e))?;
                let name = entry.file_name().into_string().map_err(|_| {
                    FsOpError::invalid(
                        "readdir",
                        &path,
                        "Invalid UTF-8 filename in directory entry",
                    )
                })?;
                if with_file_types {
                    let file_type = entry
                        .file_type()
                        .map_err(|e| FsOpError::from_io("readdir", &path, e))?;
                    entries.push(FsDirEntry {
                        name,
                        is_file: file_type.is_file(),
                        is_dir: file_type.is_dir(),
                        is_symlink: file_type.is_symlink(),
                    });
                } else {
                    names.push(name);
                }
            }
            if with_file_types {
                Ok(FsOpResult::DirEntries(entries))
            } else {
                Ok(FsOpResult::Strings(names))
            }
        }
        FsOp::Opendir { path } => {
            crate::security::require_fs_read(&path)
                .map_err(|e| FsOpError::security("opendir", &path, e))?;
            let reader =
                std::fs::read_dir(&path).map_err(|e| FsOpError::from_io("opendir", &path, e))?;
            let handle_id = store_dir_handle(&path, reader)?;
            Ok(FsOpResult::DirHandle { handle_id, path })
        }
        FsOp::ReadDirHandle { handle_id } => {
            let entry = read_dir_handle_next_sync(handle_id)?;
            Ok(FsOpResult::DirEntry(entry))
        }
        FsOp::CloseDirHandle { handle_id } => {
            close_dir_handle(handle_id)?;
            Ok(FsOpResult::Unit)
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
        FsOp::Cp { src, dst, options } => {
            crate::security::require_fs_read(&src)
                .map_err(|e| FsOpError::security("cp", &src, e))?;
            crate::security::require_fs_write(&dst)
                .map_err(|e| FsOpError::security("cp", &dst, e))?;
            cp_sync(&src, &dst, options)?;
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
        FsOp::ReadHandle {
            handle_id,
            length,
            position,
        } => {
            let bytes = read_handle_sync(handle_id, length, position)?;
            Ok(FsOpResult::Bytes(bytes))
        }
        FsOp::WriteHandle {
            handle_id,
            bytes,
            position,
        } => {
            let written = write_handle_sync(handle_id, &bytes, position)?;
            Ok(FsOpResult::Count(written))
        }
        FsOp::ReadFileHandle { handle_id } => {
            let bytes = read_file_handle_sync(handle_id)?;
            Ok(FsOpResult::Bytes(bytes))
        }
        FsOp::WriteFileHandle { handle_id, bytes } => {
            write_file_handle_sync(handle_id, &bytes)?;
            Ok(FsOpResult::Unit)
        }
        FsOp::StatHandle { handle_id } => {
            let metadata = stat_handle_sync(handle_id)?;
            Ok(FsOpResult::Metadata(metadata))
        }
        FsOp::TruncateHandle { handle_id, len } => {
            truncate_handle_sync(handle_id, len)?;
            Ok(FsOpResult::Unit)
        }
        FsOp::SyncHandle { handle_id } => {
            sync_handle_sync(handle_id)?;
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
        FsOp::Readdir { path, .. } => crate::security::require_fs_read(path)
            .map_err(|e| FsOpError::security("readdir", path, e)),
        FsOp::Opendir { path } => crate::security::require_fs_read(path)
            .map_err(|e| FsOpError::security("opendir", path, e)),
        FsOp::ReadDirHandle { .. } => Ok(()),
        FsOp::CloseDirHandle { .. } => Ok(()),
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
        FsOp::ReadHandle { .. } => Ok(()),
        FsOp::WriteHandle { .. } => Ok(()),
        FsOp::ReadFileHandle { .. } => Ok(()),
        FsOp::WriteFileHandle { .. } => Ok(()),
        FsOp::StatHandle { .. } => Ok(()),
        FsOp::TruncateHandle { .. } => Ok(()),
        FsOp::SyncHandle { .. } => Ok(()),
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
        FsOp::Readdir {
            path,
            with_file_types,
        } => {
            let mut reader = tokio::fs::read_dir(&path)
                .await
                .map_err(|e| FsOpError::from_io("readdir", &path, e))?;
            let mut names = Vec::new();
            let mut entries = Vec::new();
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
                if with_file_types {
                    let file_type = entry
                        .file_type()
                        .await
                        .map_err(|e| FsOpError::from_io("readdir", &path, e))?;
                    entries.push(FsDirEntry {
                        name,
                        is_file: file_type.is_file(),
                        is_dir: file_type.is_dir(),
                        is_symlink: file_type.is_symlink(),
                    });
                } else {
                    names.push(name);
                }
            }
            if with_file_types {
                Ok(FsOpResult::DirEntries(entries))
            } else {
                Ok(FsOpResult::Strings(names))
            }
        }
        FsOp::Opendir { path } => {
            let path_for_worker = path.clone();
            let handle_id = tokio::task::spawn_blocking(move || {
                let reader = std::fs::read_dir(&path_for_worker)
                    .map_err(|e| FsOpError::from_io("opendir", &path_for_worker, e))?;
                store_dir_handle(&path_for_worker, reader)
            })
            .await
            .map_err(|e| FsOpError::internal("opendir", e.to_string()))??;
            Ok(FsOpResult::DirHandle { handle_id, path })
        }
        FsOp::ReadDirHandle { handle_id } => {
            let entry = tokio::task::spawn_blocking(move || read_dir_handle_next_sync(handle_id))
                .await
                .map_err(|e| FsOpError::internal("read", e.to_string()))??;
            Ok(FsOpResult::DirEntry(entry))
        }
        FsOp::CloseDirHandle { handle_id } => {
            close_dir_handle(handle_id)?;
            Ok(FsOpResult::Unit)
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
        FsOp::Cp { src, dst, options } => {
            let src_for_worker = src.clone();
            let dst_for_worker = dst.clone();
            tokio::task::spawn_blocking(move || cp_sync(&src_for_worker, &dst_for_worker, options))
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
        FsOp::ReadHandle {
            handle_id,
            length,
            position,
        } => {
            let bytes =
                tokio::task::spawn_blocking(move || read_handle_sync(handle_id, length, position))
                    .await
                    .map_err(|e| FsOpError::internal("read", e.to_string()))??;
            Ok(FsOpResult::Bytes(bytes))
        }
        FsOp::WriteHandle {
            handle_id,
            bytes,
            position,
        } => {
            let written =
                tokio::task::spawn_blocking(move || write_handle_sync(handle_id, &bytes, position))
                    .await
                    .map_err(|e| FsOpError::internal("write", e.to_string()))??;
            Ok(FsOpResult::Count(written))
        }
        FsOp::ReadFileHandle { handle_id } => {
            let bytes = tokio::task::spawn_blocking(move || read_file_handle_sync(handle_id))
                .await
                .map_err(|e| FsOpError::internal("readFile", e.to_string()))??;
            Ok(FsOpResult::Bytes(bytes))
        }
        FsOp::WriteFileHandle { handle_id, bytes } => {
            tokio::task::spawn_blocking(move || write_file_handle_sync(handle_id, &bytes))
                .await
                .map_err(|e| FsOpError::internal("writeFile", e.to_string()))??;
            Ok(FsOpResult::Unit)
        }
        FsOp::StatHandle { handle_id } => {
            let metadata = tokio::task::spawn_blocking(move || stat_handle_sync(handle_id))
                .await
                .map_err(|e| FsOpError::internal("stat", e.to_string()))??;
            Ok(FsOpResult::Metadata(metadata))
        }
        FsOp::TruncateHandle { handle_id, len } => {
            tokio::task::spawn_blocking(move || truncate_handle_sync(handle_id, len))
                .await
                .map_err(|e| FsOpError::internal("truncate", e.to_string()))??;
            Ok(FsOpResult::Unit)
        }
        FsOp::SyncHandle { handle_id } => {
            tokio::task::spawn_blocking(move || sync_handle_sync(handle_id))
                .await
                .map_err(|e| FsOpError::internal("sync", e.to_string()))??;
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
    use tempfile::tempdir;

    #[test]
    fn file_handle_write_file_keeps_existing_prefix() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fh.txt");
        std::fs::write(&path, b"abcdef").expect("write seed");

        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.read(true).write(true);
        let file = open_opts.open(&path).expect("open");
        let handle_id = store_file_handle(file).expect("store handle");

        write_handle_sync(handle_id, b"ZZ", Some(2)).expect("write handle");
        write_file_handle_sync(handle_id, b"XY").expect("writeFile handle");
        close_file_handle(handle_id).expect("close");

        let out = std::fs::read(&path).expect("read");
        assert_eq!(out, b"XYZZef");
    }

    #[test]
    fn cp_force_and_error_on_exist_behavior() {
        let dir = tempdir().expect("tempdir");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"source").expect("write src");
        std::fs::write(&dst, b"existing").expect("write dst");

        let keep_opts = FsCpOptions {
            force: false,
            ..FsCpOptions::default()
        };
        cp_sync(
            src.to_str().expect("src path"),
            dst.to_str().expect("dst path"),
            keep_opts,
        )
        .expect("cp keep");
        assert_eq!(std::fs::read(&dst).expect("read dst"), b"existing");

        let err_opts = FsCpOptions {
            force: false,
            error_on_exist: true,
            ..FsCpOptions::default()
        };
        let err = cp_sync(
            src.to_str().expect("src path"),
            dst.to_str().expect("dst path"),
            err_opts,
        )
        .expect_err("cp should fail");
        assert_eq!(err.code, "EEXIST");

        cp_sync(
            src.to_str().expect("src path"),
            dst.to_str().expect("dst path"),
            FsCpOptions::default(),
        )
        .expect("cp force");
        assert_eq!(std::fs::read(&dst).expect("read dst"), b"source");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn cp_symlink_respects_dereference_option() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("target.txt");
        let src_link = dir.path().join("src-link");
        let preserved_dst = dir.path().join("dst-link");
        let deref_dst = dir.path().join("dst-file.txt");
        std::fs::write(&target, b"linkdata").expect("write target");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &src_link).expect("make symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &src_link).expect("make symlink");

        let preserve_opts = FsCpOptions {
            dereference: false,
            ..FsCpOptions::default()
        };
        cp_sync(
            src_link.to_str().expect("src path"),
            preserved_dst.to_str().expect("dst path"),
            preserve_opts,
        )
        .expect("cp symlink preserve");
        let preserved_meta = std::fs::symlink_metadata(&preserved_dst).expect("lstat");
        assert!(preserved_meta.file_type().is_symlink());

        let deref_opts = FsCpOptions {
            dereference: true,
            ..FsCpOptions::default()
        };
        cp_sync(
            src_link.to_str().expect("src path"),
            deref_dst.to_str().expect("dst path"),
            deref_opts,
        )
        .expect("cp symlink dereference");
        let deref_meta = std::fs::symlink_metadata(&deref_dst).expect("lstat");
        assert!(!deref_meta.file_type().is_symlink());
        assert_eq!(std::fs::read(&deref_dst).expect("read"), b"linkdata");
    }

    #[test]
    fn cp_mode_exclusive_rejects_existing_destination() {
        let dir = tempdir().expect("tempdir");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"src").expect("write src");
        std::fs::write(&dst, b"dst").expect("write dst");

        let opts = FsCpOptions {
            mode: 0x1,
            ..FsCpOptions::default()
        };
        let err = cp_sync(
            src.to_str().expect("src path"),
            dst.to_str().expect("dst path"),
            opts,
        )
        .expect_err("cp should fail with exclusive mode");
        assert_eq!(err.code, "EEXIST");
    }

    #[test]
    fn cp_preserve_timestamps_keeps_mtime_for_files() {
        let dir = tempdir().expect("tempdir");
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"src").expect("write src");

        let old = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_times(&src, old, old).expect("set source times");

        let opts = FsCpOptions {
            preserve_timestamps: true,
            ..FsCpOptions::default()
        };
        cp_sync(
            src.to_str().expect("src path"),
            dst.to_str().expect("dst path"),
            opts,
        )
        .expect("cp");

        let src_meta = std::fs::metadata(&src).expect("src meta");
        let dst_meta = std::fs::metadata(&dst).expect("dst meta");
        let src_mtime = filetime::FileTime::from_last_modification_time(&src_meta);
        let dst_mtime = filetime::FileTime::from_last_modification_time(&dst_meta);
        assert_eq!(src_mtime, dst_mtime);
    }

    #[test]
    fn opendir_handle_reads_entries_and_closes() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(root.join("a.txt"), b"a").expect("write a");
        std::fs::write(root.join("b.txt"), b"b").expect("write b");

        let reader = std::fs::read_dir(&root).expect("read_dir");
        let handle_id = store_dir_handle(&root.to_string_lossy(), reader).expect("store handle");

        let mut names = Vec::new();
        loop {
            match read_dir_handle_next_sync(handle_id).expect("read") {
                Some(entry) => names.push(entry.name),
                None => break,
            }
        }

        close_dir_handle(handle_id).expect("close");
        names.sort();
        assert_eq!(names, vec!["a.txt".to_string(), "b.txt".to_string()]);
    }

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
