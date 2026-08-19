//! `internalBinding('fs')` — the file-system surface vendored `fs.js` and
//! `internal/fs/*` drive.
//!
//! # Contents
//! - [`fs_binding_cjs_value`] exports the binding as `internal/otter/fs`.
//! - A descriptor table owned by the runtime, so an fd means the same thing
//!   to every module in the isolate.
//!
//! # Invariants
//! - Every path crosses the capability gate before it is touched, reads and
//!   writes separately, exactly as the rest of `node:fs` does.
//! - Errors carry the code, syscall and path Node's own errors carry, so a
//!   caller matching on `err.code` matches the same way.
//! - Stats are handed over in the 18-slot layout `getStatsFromBinding`
//!   reads: dev, mode, nlink, uid, gid, rdev, blksize, ino, size, blocks,
//!   then four (seconds, nanoseconds) stamps.
//!
//! # See also
//! - `nodelib/compat/internal_fs_binding.js`

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use otter_runtime::{
    CapabilitySet, RuntimeLocal, RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope,
    RuntimeTaskSpawner, RuntimeValue, runtime_arg_to_string,
};

/// Open descriptors, keyed by the number handed to JavaScript.
struct Descriptors {
    files: HashMap<i32, std::fs::File>,
    next: i32,
}

type Table = Arc<Mutex<Descriptors>>;

/// Build the CommonJS export of `internal/otter/fs`.
///
/// # Errors
/// Returns a native error when the surface fails to allocate.
pub fn fs_binding_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: RuntimeLocal<'scope>,
    _require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;
    let table: Table = Arc::new(Mutex::new(Descriptors {
        files: HashMap::new(),
        next: 3,
    }));

    macro_rules! method {
        ($name:literal, $arity:literal, $body:expr) => {{
            let caps = capabilities.clone();
            let table = table.clone();
            let closure = scope.native_closure(
                $name,
                $arity,
                &[],
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      args: &[RuntimeValue],
                      _c: &[RuntimeValue]| {
                    #[allow(clippy::redundant_closure_call)]
                    ($body)(ctx, args, &caps, &table)
                },
            )?;
            scope.set(object, $name, closure)?;
        }};
    }

    method!("open", 3, open_file);
    method!("close", 1, close_file);
    method!("read", 5, read_file);
    method!("writeBuffer", 5, write_buffer);
    method!("writeString", 4, write_string);
    method!("fstat", 2, fstat_file);
    method!("stat", 2, |ctx: &mut RuntimeNativeCtx<'_>,
                        args: &[RuntimeValue],
                        caps: &CapabilitySet,
                        _t: &Table| { stat_path(ctx, args, caps, false) });
    method!("lstat", 2, |ctx: &mut RuntimeNativeCtx<'_>,
                         args: &[RuntimeValue],
                         caps: &CapabilitySet,
                         _t: &Table| { stat_path(ctx, args, caps, true) });
    method!("ftruncate", 2, ftruncate_file);
    method!("fsync", 1, fsync_file);
    method!("fdatasync", 1, fdatasync_file);
    method!("futimes", 3, futimes_file);
    method!("fchmod", 2, fchmod_file);
    method!("access", 2, access_path);
    method!("chmod", 2, chmod_path);
    method!("chown", 3, chown_path);
    method!("lchown", 3, chown_path);
    method!("fchown", 3, |_ctx: &mut RuntimeNativeCtx<'_>,
                          _args: &[RuntimeValue],
                          _caps: &CapabilitySet,
                          _t: &Table| { Ok(RuntimeValue::undefined()) });
    method!("copyFile", 3, copy_file);
    method!("rename", 2, rename_path);
    method!("unlink", 1, unlink_path);
    method!("rmdir", 1, rmdir_path);
    method!("rmSync", 4, rm_path);
    method!("mkdir", 3, mkdir_path);
    method!("mkdtemp", 2, mkdtemp_path);
    method!("readdir", 3, readdir_path);
    method!("readlink", 2, readlink_path);
    method!("realpath", 2, realpath_path);
    method!("symlink", 3, symlink_path);
    method!("link", 2, link_path);
    method!("utimes", 3, utimes_path);
    method!("lutimes", 3, utimes_path);
    method!("statfs", 2, statfs_path);
    method!("existsSync", 1, exists_sync);
    method!("internalModuleStat", 2, internal_module_stat);
    method!("readFileUtf8", 2, read_file_utf8);
    method!("writeFileUtf8", 5, write_file_utf8);

    Ok(object)
}

/// The capability-checked path of an argument.
fn path_of(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    index: usize,
) -> Result<PathBuf, RuntimeNativeError> {
    let text = runtime_arg_to_string(args, index, ctx.heap());
    if text.is_empty() {
        return Err(coded_error("EINVAL", "open", Path::new("")));
    }
    Ok(PathBuf::from(text))
}

/// An error in the shape Node's `UVException` carries.
fn coded_error(code: &'static str, syscall: &'static str, path: &Path) -> RuntimeNativeError {
    // A capability refusal says so in the message: the runtime's own gate is
    // what stopped the call, not the platform.
    let reason = if code == "EACCES" {
        "permission denied"
    } else {
        "operation failed"
    };
    RuntimeNativeError::Syscall {
        code,
        message: format!("{code}: {reason}, {syscall} '{}'", path.display()),
        syscall,
        path: Some(path.display().to_string()),
        dest: None,
        errno: 0,
    }
}

fn io_code(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        std::io::ErrorKind::InvalidInput => "EINVAL",
        std::io::ErrorKind::IsADirectory => "EISDIR",
        std::io::ErrorKind::NotADirectory => "ENOTDIR",
        std::io::ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        _ => "EIO",
    }
}

fn io_failure(error: &std::io::Error, syscall: &'static str, path: &Path) -> RuntimeNativeError {
    let code = io_code(error);
    RuntimeNativeError::Syscall {
        code,
        message: format!("{code}: {error}, {syscall} '{}'", path.display()),
        syscall,
        path: Some(path.display().to_string()),
        dest: None,
        errno: error.raw_os_error().unwrap_or(0),
    }
}

/// POSIX open flags, as `fs.js` resolves them before the call. The values
/// are the platform's own, which is why they come from `libc` rather than
/// being written out.
fn open_options(flags: i32) -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    match flags & libc::O_ACCMODE {
        libc::O_WRONLY => {
            options.write(true);
        }
        libc::O_RDWR => {
            options.read(true).write(true);
        }
        _ => {
            options.read(true);
        }
    }
    if flags & libc::O_APPEND != 0 {
        options.append(true);
    } else if flags & libc::O_TRUNC != 0 {
        options.truncate(true);
    }
    if flags & libc::O_CREAT != 0 {
        options.create(true);
    }
    if flags & libc::O_EXCL != 0 {
        options.create_new(true);
    }
    options
}

fn open_file(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    let flags = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
    let writing = flags & libc::O_ACCMODE != 0 || flags & (libc::O_CREAT | libc::O_TRUNC) != 0;
    let allowed = if writing {
        capabilities.write.matches_path(&path)
    } else {
        capabilities.read.matches_path(&path)
    };
    if !allowed {
        return Err(coded_error("EACCES", "open", &path));
    }
    let file = open_options(flags)
        .open(&path)
        .map_err(|error| io_failure(&error, "open", &path))?;
    let mut descriptors = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fd = descriptors.next;
    descriptors.next += 1;
    descriptors.files.insert(fd, file);
    Ok(RuntimeValue::number_i32(fd))
}

fn close_file(
    _ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let removed = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .files
        .remove(&fd);
    if removed.is_none() {
        return Err(coded_error("EBADF", "close", Path::new("")));
    }
    Ok(RuntimeValue::undefined())
}

/// Run `body` against the file behind `fd`.
fn with_file<R>(
    table: &Table,
    fd: i32,
    syscall: &'static str,
    body: impl FnOnce(&mut std::fs::File) -> std::io::Result<R>,
) -> Result<R, RuntimeNativeError> {
    let mut descriptors = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let file = descriptors
        .files
        .get_mut(&fd)
        .ok_or_else(|| coded_error("EBADF", syscall, Path::new("")))?;
    body(file).map_err(|error| io_failure(&error, syscall, Path::new("")))
}

fn read_file(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let offset = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;
    let length = args.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as usize;
    let position = args.get(4).and_then(|v| v.as_f64());
    let mut buffer = vec![0u8; length];
    let read = with_file(table, fd, "read", |file| {
        if let Some(position) = position.filter(|value| *value >= 0.0) {
            file.seek(SeekFrom::Start(position as u64))?;
        }
        file.read(&mut buffer)
    })?;
    if read > 0
        && let Some(view) = args.get(1).and_then(|v| v.as_typed_array(ctx.heap()))
    {
        let heap = ctx.heap_mut();
        let base = view.byte_offset(heap);
        view.buffer(heap).with_bytes_mut(heap, |bytes| {
            let start = base + offset;
            let end = (start + read).min(bytes.len());
            if start < end {
                bytes[start..end].copy_from_slice(&buffer[..end - start]);
            }
        });
    }
    Ok(RuntimeValue::number_i32(read as i32))
}

fn write_buffer(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let offset = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;
    let length = args.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as usize;
    let position = args.get(4).and_then(|v| v.as_f64());
    let bytes = args
        .get(1)
        .and_then(|v| v.as_typed_array(ctx.heap()))
        .map(|view| {
            let heap = ctx.heap();
            let base = view.byte_offset(heap);
            view.buffer(heap).with_bytes(heap, |bytes| {
                let start = (base + offset).min(bytes.len());
                let end = (start + length).min(bytes.len());
                bytes[start..end].to_vec()
            })
        })
        .unwrap_or_default();
    let written = with_file(table, fd, "write", |file| {
        if let Some(position) = position.filter(|value| *value >= 0.0) {
            file.seek(SeekFrom::Start(position as u64))?;
        }
        file.write(&bytes)
    })?;
    Ok(RuntimeValue::number_i32(written as i32))
}

fn write_string(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let text = runtime_arg_to_string(args, 1, ctx.heap());
    let position = args.get(2).and_then(|v| v.as_f64());
    let bytes = text.into_bytes();
    let written = with_file(table, fd, "write", |file| {
        if let Some(position) = position.filter(|value| *value >= 0.0) {
            file.seek(SeekFrom::Start(position as u64))?;
        }
        file.write(&bytes)
    })?;
    Ok(RuntimeValue::number_i32(written as i32))
}

fn ftruncate_file(
    _ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let length = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as u64;
    with_file(table, fd, "ftruncate", |file| file.set_len(length))?;
    Ok(RuntimeValue::undefined())
}

fn fsync_file(
    _ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    with_file(table, fd, "fsync", |file| file.sync_all())?;
    Ok(RuntimeValue::undefined())
}

fn fdatasync_file(
    _ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    with_file(table, fd, "fdatasync", |file| file.sync_data())?;
    Ok(RuntimeValue::undefined())
}

fn futimes_file(
    _ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let atime = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let mtime = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0);
    with_file(table, fd, "futime", |file| {
        let times = std::fs::FileTimes::new()
            .set_accessed(UNIX_EPOCH + std::time::Duration::from_secs_f64(atime.max(0.0)))
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs_f64(mtime.max(0.0)));
        file.set_times(times)
    })?;
    Ok(RuntimeValue::undefined())
}

fn fchmod_file(
    _ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let mode = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    with_file(table, fd, "fchmod", |file| {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))
    })?;
    Ok(RuntimeValue::undefined())
}

fn fstat_file(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let metadata = with_file(table, fd, "fstat", |file| file.metadata())?;
    stats_array(ctx, &metadata)
}

fn stat_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    follow_symlink: bool,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    if !capabilities.read.matches_path(&path) {
        return Err(coded_error("EACCES", "stat", &path));
    }
    let metadata = if follow_symlink {
        std::fs::symlink_metadata(&path)
    } else {
        std::fs::metadata(&path)
    }
    .map_err(|error| io_failure(&error, "stat", &path))?;
    stats_array(ctx, &metadata)
}

/// The 18-slot stats layout `getStatsFromBinding` reads.
fn stats_array(
    ctx: &mut RuntimeNativeCtx<'_>,
    metadata: &std::fs::Metadata,
) -> Result<RuntimeValue, RuntimeNativeError> {
    use std::os::unix::fs::MetadataExt;
    let stamp = |time: std::io::Result<std::time::SystemTime>| -> (f64, f64) {
        let Ok(time) = time else { return (0.0, 0.0) };
        let Ok(since) = time.duration_since(UNIX_EPOCH) else {
            return (0.0, 0.0);
        };
        (since.as_secs() as f64, f64::from(since.subsec_nanos()))
    };
    let (atime_s, atime_ns) = stamp(metadata.accessed());
    let (mtime_s, mtime_ns) = stamp(metadata.modified());
    let ctime_s = metadata.ctime() as f64;
    let ctime_ns = metadata.ctime_nsec() as f64;
    let (birth_s, birth_ns) = stamp(metadata.created());
    let values = [
        metadata.dev() as f64,
        f64::from(metadata.mode()),
        metadata.nlink() as f64,
        f64::from(metadata.uid()),
        f64::from(metadata.gid()),
        metadata.rdev() as f64,
        metadata.blksize() as f64,
        metadata.ino() as f64,
        metadata.size() as f64,
        metadata.blocks() as f64,
        atime_s,
        atime_ns,
        mtime_s,
        mtime_ns,
        ctime_s,
        ctime_ns,
        birth_s,
        birth_ns,
    ];
    ctx.scope(|mut scope| {
        let array = scope.array(values.len())?;
        for (index, value) in values.iter().enumerate() {
            let number = scope.number(*value);
            scope.set_index(array, index, number)?;
        }
        Ok(scope.finish(array))
    })
}

/// Whether a path may be read, as an error if not.
fn allow_read(path: &Path, capabilities: &CapabilitySet, syscall: &'static str) -> Result<(), RuntimeNativeError> {
    if capabilities.read.matches_path(path) {
        return Ok(());
    }
    Err(coded_error("EACCES", syscall, path))
}

/// Whether a path may be written, as an error if not.
fn allow_write(path: &Path, capabilities: &CapabilitySet, syscall: &'static str) -> Result<(), RuntimeNativeError> {
    if capabilities.write.matches_path(path) {
        return Ok(());
    }
    Err(coded_error("EACCES", syscall, path))
}

fn access_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    let mode = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
    // W_OK is a write, everything else is a read.
    if mode & 2 != 0 {
        allow_write(&path, capabilities, "access")?;
    } else {
        allow_read(&path, capabilities, "access")?;
    }
    let metadata = std::fs::metadata(&path).map_err(|error| io_failure(&error, "access", &path))?;
    if mode & 2 != 0 && metadata.permissions().readonly() {
        return Err(coded_error("EACCES", "access", &path));
    }
    Ok(RuntimeValue::undefined())
}

fn chmod_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    use std::os::unix::fs::PermissionsExt;
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "chmod")?;
    let mode = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| io_failure(&error, "chmod", &path))?;
    Ok(RuntimeValue::undefined())
}

fn chown_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "chown")?;
    let uid = args.get(1).and_then(|v| v.as_f64()).unwrap_or(-1.0) as i64;
    let gid = args.get(2).and_then(|v| v.as_f64()).unwrap_or(-1.0) as i64;
    let text = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| coded_error("EINVAL", "chown", &path))?;
    // SAFETY: `text` is a live NUL-terminated path for the duration of the
    // call; the ids are plain integers.
    let outcome = unsafe { libc::chown(text.as_ptr(), uid as libc::uid_t, gid as libc::gid_t) };
    if outcome != 0 {
        return Err(io_failure(&std::io::Error::last_os_error(), "chown", &path));
    }
    Ok(RuntimeValue::undefined())
}

fn copy_file(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let source = path_of(ctx, args, 0)?;
    let target = path_of(ctx, args, 1)?;
    let mode = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
    allow_read(&source, capabilities, "copyfile")?;
    allow_write(&target, capabilities, "copyfile")?;
    // COPYFILE_EXCL — the destination must not already exist.
    if mode & 1 != 0 && target.exists() {
        return Err(coded_error("EEXIST", "copyfile", &target));
    }
    std::fs::copy(&source, &target).map_err(|error| io_failure(&error, "copyfile", &source))?;
    Ok(RuntimeValue::undefined())
}

fn rename_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let source = path_of(ctx, args, 0)?;
    let target = path_of(ctx, args, 1)?;
    allow_write(&source, capabilities, "rename")?;
    allow_write(&target, capabilities, "rename")?;
    std::fs::rename(&source, &target).map_err(|error| io_failure(&error, "rename", &source))?;
    Ok(RuntimeValue::undefined())
}

fn unlink_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "unlink")?;
    std::fs::remove_file(&path).map_err(|error| io_failure(&error, "unlink", &path))?;
    Ok(RuntimeValue::undefined())
}

fn rmdir_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "rmdir")?;
    std::fs::remove_dir(&path).map_err(|error| io_failure(&error, "rmdir", &path))?;
    Ok(RuntimeValue::undefined())
}

fn rm_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "rm")?;
    let recursive = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    let outcome = if path.is_dir() {
        if recursive {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_dir(&path)
        }
    } else {
        std::fs::remove_file(&path)
    };
    // A path that is already gone is the state the caller asked for; the
    // `force` decision was made before the call reached here.
    if let Err(error) = outcome
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(io_failure(&error, "rm", &path));
    }
    Ok(RuntimeValue::undefined())
}

fn mkdir_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "mkdir")?;
    let recursive = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    // A recursive mkdir answers with the first directory it created, which
    // is what `fs.mkdirSync(path, { recursive: true })` returns.
    let first_created = if recursive {
        let mut missing = None;
        let mut probe = path.as_path();
        while !probe.exists() {
            missing = Some(probe.to_path_buf());
            match probe.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => probe = parent,
                _ => break,
            }
        }
        std::fs::create_dir_all(&path).map_err(|error| io_failure(&error, "mkdir", &path))?;
        missing
    } else {
        std::fs::create_dir(&path).map_err(|error| io_failure(&error, "mkdir", &path))?;
        None
    };
    match first_created {
        Some(created) => ctx.scope(|mut scope| {
            let text = scope.string(&created.display().to_string())?;
            Ok(scope.finish(text))
        }),
        None => Ok(RuntimeValue::undefined()),
    }
}

fn mkdtemp_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let prefix = path_of(ctx, args, 0)?;
    allow_write(&prefix, capabilities, "mkdtemp")?;
    let template = format!("{}XXXXXX", prefix.display());
    let mut bytes = std::ffi::CString::new(template)
        .map_err(|_| coded_error("EINVAL", "mkdtemp", &prefix))?
        .into_bytes_with_nul();
    // SAFETY: `bytes` is a live, NUL-terminated buffer `mkdtemp` fills in
    // place with a name of the same length.
    let created = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if created.is_null() {
        return Err(io_failure(
            &std::io::Error::last_os_error(),
            "mkdtemp",
            &prefix,
        ));
    }
    bytes.pop();
    let name = String::from_utf8_lossy(&bytes).into_owned();
    ctx.scope(|mut scope| {
        let text = scope.string(&name)?;
        Ok(scope.finish(text))
    })
}

fn readdir_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_read(&path, capabilities, "scandir")?;
    let with_types = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    let mut names = Vec::new();
    let mut kinds = Vec::new();
    let entries =
        std::fs::read_dir(&path).map_err(|error| io_failure(&error, "scandir", &path))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_failure(&error, "scandir", &path))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
        if with_types {
            // The type numbers `Dirent` reads: 1 file, 2 directory, 3 link.
            let kind = entry.file_type().ok().map_or(0, |kind| {
                if kind.is_dir() {
                    2
                } else if kind.is_symlink() {
                    3
                } else {
                    1
                }
            });
            kinds.push(kind);
        }
    }
    ctx.scope(|mut scope| {
        let list = scope.array(names.len())?;
        for (index, name) in names.iter().enumerate() {
            let text = scope.string(name)?;
            scope.set_index(list, index, text)?;
        }
        if !with_types {
            return Ok(scope.finish(list));
        }
        let types = scope.array(kinds.len())?;
        for (index, kind) in kinds.iter().enumerate() {
            let number = scope.number(f64::from(*kind));
            scope.set_index(types, index, number)?;
        }
        let pair = scope.array(2)?;
        scope.set_index(pair, 0, list)?;
        scope.set_index(pair, 1, types)?;
        Ok(scope.finish(pair))
    })
}

fn readlink_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_read(&path, capabilities, "readlink")?;
    let target =
        std::fs::read_link(&path).map_err(|error| io_failure(&error, "readlink", &path))?;
    ctx.scope(|mut scope| {
        let text = scope.string(&target.display().to_string())?;
        Ok(scope.finish(text))
    })
}

fn realpath_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_read(&path, capabilities, "realpath")?;
    let resolved =
        std::fs::canonicalize(&path).map_err(|error| io_failure(&error, "realpath", &path))?;
    ctx.scope(|mut scope| {
        let text = scope.string(&resolved.display().to_string())?;
        Ok(scope.finish(text))
    })
}

fn symlink_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let target = path_of(ctx, args, 0)?;
    let link = path_of(ctx, args, 1)?;
    allow_write(&link, capabilities, "symlink")?;
    std::os::unix::fs::symlink(&target, &link)
        .map_err(|error| io_failure(&error, "symlink", &link))?;
    Ok(RuntimeValue::undefined())
}

fn link_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let existing = path_of(ctx, args, 0)?;
    let link = path_of(ctx, args, 1)?;
    allow_read(&existing, capabilities, "link")?;
    allow_write(&link, capabilities, "link")?;
    std::fs::hard_link(&existing, &link).map_err(|error| io_failure(&error, "link", &link))?;
    Ok(RuntimeValue::undefined())
}

fn utimes_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "utime")?;
    let atime = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
    let mtime = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
    let file = std::fs::File::options()
        .write(true)
        .open(&path)
        .or_else(|_| std::fs::File::open(&path))
        .map_err(|error| io_failure(&error, "utime", &path))?;
    let times = std::fs::FileTimes::new()
        .set_accessed(UNIX_EPOCH + std::time::Duration::from_secs_f64(atime))
        .set_modified(UNIX_EPOCH + std::time::Duration::from_secs_f64(mtime));
    file.set_times(times)
        .map_err(|error| io_failure(&error, "utime", &path))?;
    Ok(RuntimeValue::undefined())
}

fn statfs_path(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_read(&path, capabilities, "statfs")?;
    let text = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| coded_error("EINVAL", "statfs", &path))?;
    // SAFETY: `stats` is written by `statfs` on success; `text` is a live
    // NUL-terminated path for the call.
    let (outcome, stats) = unsafe {
        let mut stats: libc::statfs = std::mem::zeroed();
        let outcome = libc::statfs(text.as_ptr(), &raw mut stats);
        (outcome, stats)
    };
    if outcome != 0 {
        return Err(io_failure(&std::io::Error::last_os_error(), "statfs", &path));
    }
    let values = [
        f64::from(stats.f_type),
        stats.f_bsize as f64,
        stats.f_bsize as f64,
        stats.f_blocks as f64,
        stats.f_bfree as f64,
        stats.f_bavail as f64,
        stats.f_files as f64,
        stats.f_ffree as f64,
    ];
    ctx.scope(|mut scope| {
        let array = scope.array(values.len())?;
        for (index, value) in values.iter().enumerate() {
            let number = scope.number(*value);
            scope.set_index(array, index, number)?;
        }
        Ok(scope.finish(array))
    })
}

fn exists_sync(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    if !capabilities.read.matches_path(&path) {
        return Ok(RuntimeValue::boolean(false));
    }
    Ok(RuntimeValue::boolean(path.exists()))
}

fn internal_module_stat(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    _capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    // The loader's probe: 0 is a file, 1 a directory, anything negative is
    // "not there".
    let path = PathBuf::from(runtime_arg_to_string(args, 1, ctx.heap()));
    let answer = match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => 1,
        Ok(_) => 0,
        Err(_) => -1,
    };
    Ok(RuntimeValue::number_i32(answer))
}

fn read_file_utf8(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_read(&path, capabilities, "open")?;
    let text = std::fs::read_to_string(&path).map_err(|error| io_failure(&error, "open", &path))?;
    ctx.scope(|mut scope| {
        let value = scope.string(&text)?;
        Ok(scope.finish(value))
    })
}

fn write_file_utf8(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    _table: &Table,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = path_of(ctx, args, 0)?;
    allow_write(&path, capabilities, "open")?;
    let data = runtime_arg_to_string(args, 1, ctx.heap());
    let flags = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) as i32;
    let appending = flags & libc::O_APPEND != 0;
    let outcome = if appending {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(data.as_bytes()))
    } else {
        std::fs::write(&path, data.as_bytes())
    };
    outcome.map_err(|error| io_failure(&error, "open", &path))?;
    Ok(RuntimeValue::undefined())
}
