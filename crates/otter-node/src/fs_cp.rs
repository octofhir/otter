//! The native half of `fs.cpSync` — the path checks and the recursive copy
//! that `internal/fs/cp/cp-sync.js` hands off to `internalBinding('fs')`.
//!
//! # Contents
//! - [`check_paths`] validates the operand pair and prepares the
//!   destination's parent directory.
//! - [`override_file`] replaces an existing destination file.
//! - [`copy_dir`] copies a directory tree when the caller gave no filter.
//!
//! # Invariants
//! - Every path the walk reaches crosses the capability gate, reads and
//!   writes separately: a tree copy must not become a way around the gate
//!   by following a symlink out of the allowed roots.
//! - Errors carry the `ERR_FS_CP_*` code the JavaScript half would have
//!   thrown, so `err.code` reads the same either side of the boundary.
//! - Whether a link is followed is the caller's `dereference`, applied to
//!   every entry of the tree, not only to the operands.
//!
//! # See also
//! - `nodelib/internal/fs/cp/cp-sync.js` — the caller.
//! - `nodelib/internal/fs/cp/cp.js` — the asynchronous twin, whose error
//!   shapes these mirror.

use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use otter_runtime::{CapabilitySet, RuntimeNativeCtx, RuntimeNativeError, RuntimeValue};

use crate::fs_binding::{allow_read, allow_write, io_failure, path_of};

/// One of the `ERR_FS_CP_*` failures, in the shape the JavaScript half
/// throws them.
fn cp_error(code: &'static str, message: String) -> RuntimeNativeError {
    RuntimeNativeError::Coded {
        kind: otter_vm::ErrorKind::Error,
        code,
        message,
    }
}

/// A path with the working directory applied and `.`/`..` folded away, so
/// two spellings of one location compare equal.
fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `dest` names `src` itself or something inside it, by path alone.
fn is_src_subdir(src: &Path, dest: &Path) -> bool {
    let src = absolute(src);
    let dest = absolute(dest);
    let mut dest_parts = dest.components();
    for part in src.components() {
        match dest_parts.next() {
            Some(other) if other == part => {}
            _ => return false,
        }
    }
    true
}

/// Whether two stats describe one file.
fn are_identical(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// The stat a `dereference` flag asks for.
fn stat_with(path: &Path, dereference: bool) -> std::io::Result<Metadata> {
    if dereference {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
}

/// The options `cpSyncCopyDir` is called with.
#[derive(Clone, Copy)]
struct CopyOptions {
    force: bool,
    dereference: bool,
    error_on_exist: bool,
    verbatim_symlinks: bool,
    preserve_timestamps: bool,
}

fn flag(args: &[RuntimeValue], index: usize) -> bool {
    args.get(index)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false)
}

/// `cpSyncCheckPaths(src, dest, dereference, recursive)`.
///
/// Rejects the operand pairs a copy must not accept, then makes sure the
/// destination has a parent to land in.
///
/// # Errors
/// Returns the `ERR_FS_CP_*` failure the pair earns, or the platform error
/// behind a stat that could not be taken.
pub(crate) fn check_paths(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let src = path_of(ctx, args, 0)?;
    let dest = path_of(ctx, args, 1)?;
    let dereference = flag(args, 2);
    let recursive = flag(args, 3);
    allow_read(&src, capabilities, "cp")?;
    allow_write(&dest, capabilities, "cp")?;

    let src_stat =
        stat_with(&src, dereference).map_err(|error| io_failure(&error, "lstat", &src))?;
    let dest_stat = stat_with(&dest, dereference).ok();

    if let Some(dest_stat) = &dest_stat {
        if are_identical(&src_stat, dest_stat) {
            return Err(cp_error(
                "ERR_FS_CP_EINVAL",
                format!("src and dest cannot be the same {}", src.display()),
            ));
        }
        if src_stat.is_dir() && !dest_stat.is_dir() {
            return Err(cp_error(
                "ERR_FS_CP_DIR_TO_NON_DIR",
                format!(
                    "cannot overwrite non-directory {} with directory {}",
                    dest.display(),
                    src.display()
                ),
            ));
        }
        if !src_stat.is_dir() && dest_stat.is_dir() {
            return Err(cp_error(
                "ERR_FS_CP_NON_DIR_TO_DIR",
                format!(
                    "cannot overwrite directory {} with non-directory {}",
                    dest.display(),
                    src.display()
                ),
            ));
        }
    }

    if src_stat.is_dir() && is_src_subdir(&src, &dest) {
        return Err(subdirectory_of_self(&src, &dest));
    }
    check_parent_paths(&src, &src_stat, &dest)?;

    if src_stat.is_dir() {
        if !recursive {
            return Err(cp_error(
                "ERR_FS_EISDIR",
                format!("{} is a directory (not copied)", src.display()),
            ));
        }
    } else if !src_stat.is_file() && !src_stat.file_type().is_symlink() {
        return Err(unsupported_source(&src_stat, &dest));
    }

    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        allow_write(parent, capabilities, "mkdir")?;
        std::fs::create_dir_all(parent).map_err(|error| io_failure(&error, "mkdir", parent))?;
    }
    Ok(RuntimeValue::undefined())
}

fn subdirectory_of_self(src: &Path, dest: &Path) -> RuntimeNativeError {
    cp_error(
        "ERR_FS_CP_EINVAL",
        format!(
            "cannot copy {} to a subdirectory of self {}",
            src.display(),
            dest.display()
        ),
    )
}

/// The failure a source that is neither file, directory nor link earns.
fn unsupported_source(stat: &Metadata, dest: &Path) -> RuntimeNativeError {
    let kind = stat.file_type();
    if kind.is_socket() {
        cp_error(
            "ERR_FS_CP_SOCKET",
            format!("cannot copy a socket file: {}", dest.display()),
        )
    } else if kind.is_fifo() {
        cp_error(
            "ERR_FS_CP_FIFO_PIPE",
            format!("cannot copy a FIFO pipe: {}", dest.display()),
        )
    } else {
        cp_error(
            "ERR_FS_CP_UNKNOWN",
            format!("cannot copy an unknown file type: {}", dest.display()),
        )
    }
}

/// Walks the destination's parents to the root: reaching the source through
/// one of them means the copy would descend into itself, however the path
/// was spelled.
fn check_parent_paths(
    src: &Path,
    src_stat: &Metadata,
    dest: &Path,
) -> Result<(), RuntimeNativeError> {
    let src_parent = src.parent().map(absolute);
    let mut dest_parent = match dest.parent() {
        Some(parent) => absolute(parent),
        None => return Ok(()),
    };
    loop {
        if Some(&dest_parent) == src_parent.as_ref() {
            return Ok(());
        }
        match std::fs::metadata(&dest_parent) {
            Ok(stat) if are_identical(src_stat, &stat) => {
                return Err(subdirectory_of_self(src, dest));
            }
            _ => {}
        }
        match dest_parent.parent() {
            Some(parent) if parent != dest_parent => dest_parent = parent.to_path_buf(),
            _ => return Ok(()),
        }
    }
}

/// `cpSyncOverrideFile(src, dest, mode, preserveTimestamps)`.
///
/// # Errors
/// Returns the platform error behind the replacement.
pub(crate) fn override_file(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let src = path_of(ctx, args, 0)?;
    let dest = path_of(ctx, args, 1)?;
    let mode = args.get(2).and_then(|value| value.as_f64()).unwrap_or(0.0) as i32;
    let preserve_timestamps = flag(args, 3);
    allow_read(&src, capabilities, "cp")?;
    allow_write(&dest, capabilities, "cp")?;
    copy_one_file(&src, &dest, mode, preserve_timestamps)?;
    Ok(RuntimeValue::undefined())
}

/// Copies a single file, replacing what is there, and carries the source's
/// mode — and, when asked, its timestamps — over to the copy.
fn copy_one_file(
    src: &Path,
    dest: &Path,
    mode: i32,
    preserve_timestamps: bool,
) -> Result<(), RuntimeNativeError> {
    // COPYFILE_EXCL — the destination must not already exist.
    if mode & 1 != 0 && dest.symlink_metadata().is_ok() {
        return Err(crate::fs_binding::coded_error("EEXIST", "cp", dest));
    }
    if dest.symlink_metadata().is_ok() {
        std::fs::remove_file(dest).map_err(|error| io_failure(&error, "unlink", dest))?;
    }
    std::fs::copy(src, dest).map_err(|error| io_failure(&error, "copyfile", src))?;
    let src_stat = std::fs::metadata(src).map_err(|error| io_failure(&error, "stat", src))?;
    std::fs::set_permissions(
        dest,
        std::fs::Permissions::from_mode(src_stat.permissions().mode()),
    )
    .map_err(|error| io_failure(&error, "chmod", dest))?;
    if preserve_timestamps {
        set_timestamps(&src_stat, dest)?;
    }
    Ok(())
}

fn set_timestamps(src_stat: &Metadata, dest: &Path) -> Result<(), RuntimeNativeError> {
    let file = std::fs::File::options()
        .write(true)
        .open(dest)
        .map_err(|error| io_failure(&error, "utime", dest))?;
    let accessed = UNIX_EPOCH
        + std::time::Duration::new(
            src_stat.atime().max(0) as u64,
            src_stat.atime_nsec().max(0) as u32,
        );
    let modified = UNIX_EPOCH
        + std::time::Duration::new(
            src_stat.mtime().max(0) as u64,
            src_stat.mtime_nsec().max(0) as u32,
        );
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(accessed)
            .set_modified(modified),
    )
    .map_err(|error| io_failure(&error, "utime", dest))
}

/// `cpSyncCopyDir(src, dest, force, dereference, errorOnExist,
/// verbatimSymlinks, preserveTimestamps)`.
///
/// # Errors
/// Returns the first failure the walk meets.
pub(crate) fn copy_dir(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let src = path_of(ctx, args, 0)?;
    let dest = path_of(ctx, args, 1)?;
    let options = CopyOptions {
        force: flag(args, 2),
        dereference: flag(args, 3),
        error_on_exist: flag(args, 4),
        verbatim_symlinks: flag(args, 5),
        preserve_timestamps: flag(args, 6),
    };
    copy_directory(&src, &dest, options, capabilities)?;
    Ok(RuntimeValue::undefined())
}

fn copy_directory(
    src: &Path,
    dest: &Path,
    options: CopyOptions,
    capabilities: &CapabilitySet,
) -> Result<(), RuntimeNativeError> {
    allow_read(src, capabilities, "cp")?;
    allow_write(dest, capabilities, "cp")?;
    let src_stat = std::fs::metadata(src).map_err(|error| io_failure(&error, "stat", src))?;
    let existed = dest.symlink_metadata().is_ok();
    if !existed {
        std::fs::create_dir_all(dest).map_err(|error| io_failure(&error, "mkdir", dest))?;
    }

    let entries = std::fs::read_dir(src).map_err(|error| io_failure(&error, "scandir", src))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_failure(&error, "scandir", src))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        copy_entry(&from, &to, options, capabilities)?;
    }

    if !existed {
        std::fs::set_permissions(
            dest,
            std::fs::Permissions::from_mode(src_stat.permissions().mode()),
        )
        .map_err(|error| io_failure(&error, "chmod", dest))?;
    }
    Ok(())
}

fn copy_entry(
    src: &Path,
    dest: &Path,
    options: CopyOptions,
    capabilities: &CapabilitySet,
) -> Result<(), RuntimeNativeError> {
    allow_read(src, capabilities, "cp")?;
    allow_write(dest, capabilities, "cp")?;
    let src_stat =
        stat_with(src, options.dereference).map_err(|error| io_failure(&error, "lstat", src))?;
    let dest_stat = stat_with(dest, options.dereference).ok();

    if src_stat.is_dir() {
        return copy_directory(src, dest, options, capabilities);
    }
    if src_stat.file_type().is_symlink() {
        return copy_link(src, dest, dest_stat.is_some(), options);
    }
    if src_stat.is_file()
        || src_stat.file_type().is_char_device()
        || src_stat.file_type().is_block_device()
    {
        if dest_stat.is_none() {
            copy_one_file(src, dest, 0, options.preserve_timestamps)?;
            return Ok(());
        }
        if options.force {
            return copy_one_file(src, dest, 0, options.preserve_timestamps);
        }
        if options.error_on_exist {
            return Err(cp_error(
                "ERR_FS_CP_EEXIST",
                format!("{} already exists", dest.display()),
            ));
        }
        return Ok(());
    }
    Err(unsupported_source(&src_stat, dest))
}

/// A symlink is copied as a link: the target is rewritten to an absolute
/// path unless the caller asked for it verbatim.
fn copy_link(
    src: &Path,
    dest: &Path,
    dest_exists: bool,
    options: CopyOptions,
) -> Result<(), RuntimeNativeError> {
    let mut resolved_src =
        std::fs::read_link(src).map_err(|error| io_failure(&error, "readlink", src))?;
    if !options.verbatim_symlinks
        && !resolved_src.is_absolute()
        && let Some(parent) = src.parent()
    {
        resolved_src = absolute(&parent.join(&resolved_src));
    }
    if !dest_exists {
        return std::os::unix::fs::symlink(&resolved_src, dest)
            .map_err(|error| io_failure(&error, "symlink", dest));
    }

    let resolved_dest = match std::fs::read_link(dest) {
        Ok(target) => {
            if target.is_absolute() {
                target
            } else if let Some(parent) = dest.parent() {
                absolute(&parent.join(target))
            } else {
                target
            }
        }
        // The destination is a regular file or directory, not a link: the
        // symlink call below reports the collision.
        Err(_) => {
            return std::os::unix::fs::symlink(&resolved_src, dest)
                .map_err(|error| io_failure(&error, "symlink", dest));
        }
    };

    let src_is_dir = std::fs::metadata(src).is_ok_and(|stat| stat.is_dir());
    if src_is_dir && is_src_subdir(&resolved_src, &resolved_dest) {
        return Err(subdirectory_of_self(&resolved_src, &resolved_dest));
    }
    if std::fs::metadata(dest).is_ok_and(|stat| stat.is_dir())
        && is_src_subdir(&resolved_dest, &resolved_src)
    {
        return Err(cp_error(
            "ERR_FS_CP_SYMLINK_TO_SUBDIRECTORY",
            format!(
                "cannot overwrite {} with {}",
                resolved_dest.display(),
                resolved_src.display()
            ),
        ));
    }
    std::fs::remove_file(dest).map_err(|error| io_failure(&error, "unlink", dest))?;
    std::os::unix::fs::symlink(&resolved_src, dest)
        .map_err(|error| io_failure(&error, "symlink", dest))
}
