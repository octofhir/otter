//! `node:fs` native core.
//!
//! Provides the capability-gated synchronous primitives that the `fs.js` shim
//! builds the full Node `fs` surface on top of. Raw file bytes cross the
//! native/JS boundary as latin1 strings (each byte maps 1:1 to a code unit), so
//! the JS layer can wrap them in a `Buffer` and apply any encoding.
//!
//! # Contents
//! - [`install_fs_module`] - the legacy ESM namespace (text sync helpers).
//! - [`fs_native_value`] - the raw sync core consumed by `fs.js`.
//!
//! # Invariants
//! - Every operation checks `read`/`write` capabilities at the Rust boundary
//!   before touching the host filesystem.
//! - No VM state is retained across host I/O.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
    RuntimeValue as Value, runtime_arg_to_string, runtime_optional_arg_to_string,
};

/// Errors produced by the native `node:fs` core.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// Filesystem permission denied.
    #[error("permission denied for `{path}`")]
    PermissionDenied {
        /// Path that was rejected.
        path: PathBuf,
    },
    /// Filesystem I/O error.
    #[error("{message}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying error message.
        message: String,
        /// Node-style error code (e.g. `ENOENT`).
        code: &'static str,
    },
}

/// Result alias for `node:fs`.
pub type FsResult<T> = Result<T, FsError>;

/// Build the legacy `node:fs` / `fs` ESM namespace (text sync helpers).
pub fn install_fs_module<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    let namespace = scope.bare_object()?;
    let read_caps = caps.clone();
    let read_file_sync = scope.native_closure(
        "readFileSync",
        2,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
            let path = path_arg(ctx, args, 0, "fs.readFileSync")?;
            require_read(&path, &read_caps).map_err(fs_error)?;
            let encoding = runtime_optional_arg_to_string(args, 1, ctx.heap());
            let bytes = std::fs::read(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
            if encoding.as_deref().is_some_and(|e| !e.is_empty()) {
                crate::string_value(ctx, &String::from_utf8_lossy(&bytes))
            } else {
                crate::string_value(ctx, &bytes_to_latin1(&bytes))
            }
        },
    )?;
    scope.set(namespace, "readFileSync", read_file_sync)?;

    let write_caps = caps.clone();
    let write_file_sync = scope.native_closure(
        "writeFileSync",
        3,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
            let path = path_arg(ctx, args, 0, "fs.writeFileSync")?;
            require_write(&path, &write_caps).map_err(fs_error)?;
            let data = runtime_arg_to_string(args, 1, ctx.heap());
            std::fs::write(&path, data.as_bytes()).map_err(|e| fs_error(io_error(&path, &e)))?;
            Ok(Value::undefined())
        },
    )?;
    scope.set(namespace, "writeFileSync", write_file_sync)?;

    let exists_caps = caps.clone();
    let exists_sync = scope.native_closure(
        "existsSync",
        1,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
            let path = path_arg(ctx, args, 0, "fs.existsSync")?;
            Ok(Value::boolean(
                exists_caps.read.matches_path(&path) && path.exists(),
            ))
        },
    )?;
    scope.set(namespace, "existsSync", exists_sync)?;

    let mkdir_caps = caps.clone();
    let mkdir_sync = scope.native_closure(
        "mkdirSync",
        2,
        &[],
        move |ctx: &mut NativeCtx<'_>, args: &[Value], _c: &[Value]| {
            let path = path_arg(ctx, args, 0, "fs.mkdirSync")?;
            require_write(&path, &mkdir_caps).map_err(fs_error)?;
            let recursive = args.get(1).is_some_and(truthy);
            let r = if recursive {
                std::fs::create_dir_all(&path)
            } else {
                std::fs::create_dir(&path)
            };
            r.map_err(|e| fs_error(io_error(&path, &e)))?;
            Ok(Value::undefined())
        },
    )?;
    scope.set(namespace, "mkdirSync", mkdir_sync)?;
    Ok(namespace)
}

const FS_SHIM: &str = include_str!("fs.js");
const FS_PROMISES_SHIM: &str = "'use strict';\nmodule.exports = require('fs').promises;\n";

/// CommonJS export: the full `fs` namespace, built by `fs.js` on the native core.
pub fn fs_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:fs", FS_SHIM, module, require)
}

/// CommonJS export: `fs/promises` (= `require('fs').promises`).
pub fn fs_promises_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:fs/promises",
        FS_PROMISES_SHIM,
        module,
        require,
    )
}

/// Hidden CommonJS row that supplies the capability-gated native `fs` core.
pub fn fs_native_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    fs_native_value(scope, caps)
}

/// Build the raw synchronous core as a value: `{ readRaw, writeRaw, stat, … }`.
/// Each method captures a clone of the capability set.
pub fn fs_native_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
) -> Result<Local<'scope>, NativeError> {
    let object = scope.object()?;
    macro_rules! m {
        ($name:literal, $len:expr, $f:ident) => {{
            let caps = caps.clone();
            let method = scope.native_closure(
                $name,
                $len,
                &[],
                move |ctx: &mut NativeCtx<'_>, args: &[Value], _captures: &[Value]| {
                    $f(ctx, args, &caps)
                },
            )?;
            scope.set(object, $name, method)?;
        }};
    }

    m!("readRaw", 1, fs_read_raw);
    m!("writeRaw", 3, fs_write_raw);
    m!("existsRaw", 1, fs_exists);
    m!("statRaw", 2, fs_stat_raw);
    m!("readdirRaw", 1, fs_readdir_raw);
    m!("readdirTypes", 1, fs_readdir_types);
    m!("mkdir", 2, fs_mkdir);
    m!("rm", 3, fs_rm);
    m!("rmdir", 1, fs_rmdir);
    m!("unlink", 1, fs_unlink);
    m!("realpath", 1, fs_realpath);
    m!("copyFile", 2, fs_copy_file);
    m!("access", 2, fs_access);
    m!("rename", 2, fs_rename);
    m!("readlink", 1, fs_readlink);
    m!("chmod", 2, fs_chmod);
    m!("truncate", 2, fs_truncate);
    m!("openFd", 2, fs_open_fd);
    m!("readFd", 3, fs_read_fd);
    m!("writeFd", 3, fs_write_fd);
    m!("closeFd", 1, fs_close_fd);
    m!("fstatFd", 1, fs_fstat_fd);

    Ok(object)
}

// ---- file-descriptor table (open/read/write/close/fstat) ----

thread_local! {
    static FD_TABLE: std::cell::RefCell<std::collections::HashMap<i32, std::fs::File>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static NEXT_FD: std::cell::Cell<i32> = const { std::cell::Cell::new(3) };
}

fn open_options_for_flags(flags: &str) -> std::fs::OpenOptions {
    let mut opts = std::fs::OpenOptions::new();
    match flags {
        "r" => opts.read(true),
        "r+" => opts.read(true).write(true),
        "w" => opts.write(true).create(true).truncate(true),
        "w+" => opts.read(true).write(true).create(true).truncate(true),
        "a" => opts.append(true).create(true),
        "a+" => opts.read(true).append(true).create(true),
        "wx" | "xw" => opts.write(true).create_new(true),
        "ax" | "xa" => opts.append(true).create_new(true),
        _ => opts.read(true),
    };
    opts
}

fn fs_open_fd(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.open")?;
    let flags = args
        .get(1)
        .map(|_| runtime_arg_to_string(args, 1, ctx.heap()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "r".to_string());
    let writing = flags.contains(['w', 'a', '+']) || flags == "r+";
    if writing {
        require_write(&path, caps).map_err(fs_error)?;
    } else {
        require_read(&path, caps).map_err(fs_error)?;
    }
    let file = open_options_for_flags(&flags)
        .open(&path)
        .map_err(|e| fs_error(io_error(&path, &e)))?;
    let fd = NEXT_FD.with(|n| {
        let cur = n.get();
        n.set(cur + 1);
        cur
    });
    FD_TABLE.with(|t| t.borrow_mut().insert(fd, file));
    Ok(Value::number(otter_vm::number::NumberValue::from_i32(fd)))
}

fn fs_read_fd(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    _caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    use std::io::{Read, Seek, SeekFrom};
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let length = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0) as usize;
    let position = args.get(2).and_then(|v| v.as_f64());
    let bytes = FD_TABLE
        .with(|t| -> std::io::Result<Vec<u8>> {
            let mut table = t.borrow_mut();
            let file = table
                .get_mut(&fd)
                .ok_or_else(|| std::io::Error::from_raw_os_error(9))?;
            if let Some(pos) = position.filter(|p| *p >= 0.0) {
                file.seek(SeekFrom::Start(pos as u64))?;
            }
            let mut buf = vec![0u8; length];
            let n = file.read(&mut buf)?;
            buf.truncate(n);
            Ok(buf)
        })
        .map_err(|e| fs_error(io_error(Path::new("<fd>"), &e)))?;
    crate::string_value(ctx, &bytes_to_latin1(&bytes))
}

fn fs_write_fd(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    _caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    use std::io::{Seek, SeekFrom, Write};
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let data = latin1_to_bytes(&runtime_arg_to_string(args, 1, ctx.heap()));
    let position = args.get(2).and_then(|v| v.as_f64());
    let written = FD_TABLE
        .with(|t| -> std::io::Result<usize> {
            let mut table = t.borrow_mut();
            let file = table
                .get_mut(&fd)
                .ok_or_else(|| std::io::Error::from_raw_os_error(9))?;
            if let Some(pos) = position.filter(|p| *p >= 0.0) {
                file.seek(SeekFrom::Start(pos as u64))?;
            }
            file.write_all(&data)?;
            Ok(data.len())
        })
        .map_err(|e| fs_error(io_error(Path::new("<fd>"), &e)))?;
    Ok(Value::number(otter_vm::number::NumberValue::from_f64(
        written as f64,
    )))
}

fn fs_close_fd(
    _ctx: &mut NativeCtx<'_>,
    args: &[Value],
    _caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    FD_TABLE.with(|t| t.borrow_mut().remove(&fd));
    Ok(Value::undefined())
}

fn fs_fstat_fd(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    _caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let fd = args.first().and_then(|v| v.as_f64()).unwrap_or(-1.0) as i32;
    let meta = FD_TABLE
        .with(|t| t.borrow().get(&fd).map(std::fs::File::metadata))
        .ok_or_else(|| {
            fs_error(io_error(
                Path::new("<fd>"),
                &std::io::Error::from_raw_os_error(9),
            ))
        })?
        .map_err(|e| fs_error(io_error(Path::new("<fd>"), &e)))?;
    stat_object(ctx, &meta)
}

// ---- raw byte/latin1 bridging ----

fn truthy(v: &Value) -> bool {
    v.as_boolean().unwrap_or(false)
}

fn bytes_to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn latin1_to_bytes(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

// ---- native operations ----

fn fs_read_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readFile")?;
    require_read(&path, caps).map_err(fs_error)?;
    let bytes = std::fs::read(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    crate::string_value(ctx, &bytes_to_latin1(&bytes))
}

fn fs_write_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.writeFile")?;
    require_write(&path, caps).map_err(fs_error)?;
    let data = runtime_arg_to_string(args, 1, ctx.heap());
    let append = args.get(2).is_some_and(truthy);
    let bytes = latin1_to_bytes(&data);
    let result = if append {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&bytes))
    } else {
        std::fs::write(&path, &bytes)
    };
    result.map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_exists(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.exists")?;
    if !caps.read.matches_path(&path) {
        return Ok(Value::boolean(false));
    }
    Ok(Value::boolean(path.exists()))
}

fn fs_stat_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.stat")?;
    require_read(&path, caps).map_err(fs_error)?;
    let lstat = args.get(1).is_some_and(truthy);
    let meta = if lstat {
        std::fs::symlink_metadata(&path)
    } else {
        std::fs::metadata(&path)
    }
    .map_err(|e| fs_error(io_error(&path, &e)))?;
    stat_object(ctx, &meta)
}

/// Build the raw Stats-data object that `fs.js` wraps into a `Stats` instance.
fn stat_object(ctx: &mut NativeCtx<'_>, meta: &std::fs::Metadata) -> Result<Value, NativeError> {
    let to_ms = |t: Option<std::time::SystemTime>| -> f64 {
        t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    };
    let mtime = to_ms(meta.modified().ok());
    let atime = to_ms(meta.accessed().ok());
    let ctime = mtime;
    let birthtime = to_ms(meta.created().ok());
    let file_type = meta.file_type();

    ctx.scope(|mut scope| {
        let object = scope.object()?;
        set_number(&mut scope, object, "size", meta.len() as f64)?;
        set_number(&mut scope, object, "mode", file_mode(meta) as f64)?;
        set_number(&mut scope, object, "mtimeMs", mtime)?;
        set_number(&mut scope, object, "atimeMs", atime)?;
        set_number(&mut scope, object, "ctimeMs", ctime)?;
        set_number(&mut scope, object, "birthtimeMs", birthtime)?;
        set_bool(&mut scope, object, "isFile", file_type.is_file())?;
        set_bool(&mut scope, object, "isDirectory", file_type.is_dir())?;
        set_bool(&mut scope, object, "isSymbolicLink", file_type.is_symlink())?;
        let (blksize, blocks, dev, ino, nlink, uid, gid, rdev) = stat_extra(meta);
        set_number(&mut scope, object, "blksize", blksize)?;
        set_number(&mut scope, object, "blocks", blocks)?;
        set_number(&mut scope, object, "dev", dev)?;
        set_number(&mut scope, object, "ino", ino)?;
        set_number(&mut scope, object, "nlink", nlink)?;
        set_number(&mut scope, object, "uid", uid)?;
        set_number(&mut scope, object, "gid", gid)?;
        set_number(&mut scope, object, "rdev", rdev)?;
        Ok(scope.finish(object))
    })
}

fn fs_readdir_raw(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readdir")?;
    require_read(&path, caps).map_err(fs_error)?;
    let entries = read_dir_names(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    ctx.scope(|mut scope| {
        let array = scope.array(entries.len())?;
        for (index, name) in entries.iter().enumerate() {
            scope.scope(|mut item_scope| {
                let value = item_scope.string(name)?;
                item_scope.set_index(array, index, value)
            })?;
        }
        Ok(scope.finish(array))
    })
}

fn fs_readdir_types(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readdir")?;
    require_read(&path, caps).map_err(fs_error)?;
    let dir = std::fs::read_dir(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    let mut rows: Vec<(String, bool, bool, bool)> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type().ok();
        rows.push((
            name,
            ft.as_ref().is_some_and(std::fs::FileType::is_dir),
            ft.as_ref().is_some_and(std::fs::FileType::is_file),
            ft.as_ref().is_some_and(std::fs::FileType::is_symlink),
        ));
    }
    ctx.scope(|mut scope| {
        let array = scope.array(rows.len())?;
        for (index, (name, is_dir, is_file, is_link)) in rows.iter().enumerate() {
            scope.scope(|mut row_scope| {
                let row = row_scope.object()?;
                let name = row_scope.string(name)?;
                row_scope.set(row, "name", name)?;
                set_bool(&mut row_scope, row, "isDir", *is_dir)?;
                set_bool(&mut row_scope, row, "isFile", *is_file)?;
                set_bool(&mut row_scope, row, "isSymlink", *is_link)?;
                row_scope.set_index(array, index, row)
            })?;
        }
        Ok(scope.finish(array))
    })
}

fn fs_mkdir(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.mkdir")?;
    require_write(&path, caps).map_err(fs_error)?;
    let recursive = args.get(1).is_some_and(truthy);
    let result = if recursive {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    result.map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_rm(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.rm")?;
    require_write(&path, caps).map_err(fs_error)?;
    let recursive = args.get(1).is_some_and(truthy);
    let force = args.get(2).is_some_and(truthy);
    let result = if path.is_dir() {
        if recursive {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_dir(&path)
        }
    } else {
        std::fs::remove_file(&path)
    };
    match result {
        Ok(()) => Ok(Value::undefined()),
        Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => Ok(Value::undefined()),
        Err(e) => Err(fs_error(io_error(&path, &e))),
    }
}

fn fs_rmdir(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.rmdir")?;
    require_write(&path, caps).map_err(fs_error)?;
    let recursive = args.get(1).is_some_and(truthy);
    let result = if recursive {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_dir(&path)
    };
    result.map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_unlink(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.unlink")?;
    require_write(&path, caps).map_err(fs_error)?;
    std::fs::remove_file(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

fn fs_realpath(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.realpath")?;
    require_read(&path, caps).map_err(fs_error)?;
    let real = std::fs::canonicalize(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    crate::string_value(ctx, &real.to_string_lossy())
}

fn fs_copy_file(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let src = path_arg(ctx, args, 0, "fs.copyFile")?;
    let dest = path_arg(ctx, args, 1, "fs.copyFile")?;
    require_read(&src, caps).map_err(fs_error)?;
    require_write(&dest, caps).map_err(fs_error)?;
    std::fs::copy(&src, &dest).map_err(|e| fs_error(io_error(&src, &e)))?;
    Ok(Value::undefined())
}

fn fs_access(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.access")?;
    require_read(&path, caps).map_err(fs_error)?;
    if path.exists() {
        Ok(Value::undefined())
    } else {
        Err(fs_error(FsError::Io {
            path: path.clone(),
            message: format!(
                "ENOENT: no such file or directory, access '{}'",
                path.display()
            ),
            code: "ENOENT",
        }))
    }
}

fn fs_rename(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let from = path_arg(ctx, args, 0, "fs.rename")?;
    let to = path_arg(ctx, args, 1, "fs.rename")?;
    require_write(&from, caps).map_err(fs_error)?;
    require_write(&to, caps).map_err(fs_error)?;
    std::fs::rename(&from, &to).map_err(|e| fs_error(io_error(&from, &e)))?;
    Ok(Value::undefined())
}

fn fs_readlink(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.readlink")?;
    require_read(&path, caps).map_err(fs_error)?;
    let target = std::fs::read_link(&path).map_err(|e| fs_error(io_error(&path, &e)))?;
    crate::string_value(ctx, &target.to_string_lossy())
}

fn fs_chmod(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.chmod")?;
    require_write(&path, caps).map_err(fs_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u32;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| fs_error(io_error(&path, &e)))?;
    }
    Ok(Value::undefined())
}

fn fs_truncate(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let path = path_arg(ctx, args, 0, "fs.truncate")?;
    require_write(&path, caps).map_err(fs_error)?;
    let len = args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map_err(|e| fs_error(io_error(&path, &e)))?;
    file.set_len(len)
        .map_err(|e| fs_error(io_error(&path, &e)))?;
    Ok(Value::undefined())
}

// ---- helpers ----

fn read_dir_names(path: &Path) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(path)? {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

fn set_bool(
    scope: &mut NativeScope<'_, '_>,
    object: Local<'_>,
    key: &str,
    value: bool,
) -> Result<(), NativeError> {
    let value = scope.boolean(value);
    scope.set(object, key, value)
}

fn set_number(
    scope: &mut NativeScope<'_, '_>,
    object: Local<'_>,
    key: &str,
    value: f64,
) -> Result<(), NativeError> {
    let value = scope.number(value);
    scope.set(object, key, value)
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}
#[cfg(not(unix))]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
#[allow(clippy::type_complexity)]
fn stat_extra(meta: &std::fs::Metadata) -> (f64, f64, f64, f64, f64, f64, f64, f64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.blksize() as f64,
        meta.blocks() as f64,
        meta.dev() as f64,
        meta.ino() as f64,
        meta.nlink() as f64,
        meta.uid() as f64,
        meta.gid() as f64,
        meta.rdev() as f64,
    )
}
#[cfg(not(unix))]
fn stat_extra(_meta: &std::fs::Metadata) -> (f64, f64, f64, f64, f64, f64, f64, f64) {
    (4096.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0)
}

fn path_arg(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    index: usize,
    name: &'static str,
) -> Result<PathBuf, NativeError> {
    let path = crate::arg_string(args, index, name, ctx.heap())?;
    if path.is_empty() {
        return Err(crate::type_error(name, "path is required"));
    }
    Ok(PathBuf::from(path))
}

fn io_error(path: &Path, err: &std::io::Error) -> FsError {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        _ => "EIO",
    };
    FsError::Io {
        path: path.to_path_buf(),
        message: format!("{code}: {err}, '{}'", path.display()),
        code,
    }
}

fn require_read(path: &Path, capabilities: &CapabilitySet) -> FsResult<()> {
    if capabilities.read.matches_path(path) {
        Ok(())
    } else {
        Err(FsError::PermissionDenied {
            path: path.to_path_buf(),
        })
    }
}

fn require_write(path: &Path, capabilities: &CapabilitySet) -> FsResult<()> {
    if capabilities.write.matches_path(path) {
        Ok(())
    } else {
        Err(FsError::PermissionDenied {
            path: path.to_path_buf(),
        })
    }
}

fn fs_error(err: FsError) -> NativeError {
    let code = match &err {
        FsError::PermissionDenied { .. } => "EACCES",
        FsError::Io { code, .. } => code,
    };
    NativeError::Coded {
        kind: otter_vm::ErrorKind::Error,
        code,
        message: err.to_string(),
    }
}
