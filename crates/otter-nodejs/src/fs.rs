use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use otter_macros::lodge;
use otter_runtime::{
    ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError, current_process,
};
use otter_vm::object::{HeapValueKind, TypedArrayKind};
use otter_vm::payload::{VmTrace, VmValueTracer};

use crate::process::current_node_cwd;
use crate::support::{
    install_method, install_readonly_value, own_property, string_value, type_error, value_to_string,
};

const FS_EXPORT_SLOT: &str = "__otter_node_fs_export";
const START_FD: i32 = 100;

#[derive(Debug, Default)]
struct FsState {
    next_fd: i32,
    files: BTreeMap<i32, File>,
}

#[derive(Debug, Clone)]
struct FsPayload {
    shared: Arc<Mutex<FsState>>,
}

impl VmTrace for FsPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

lodge!(
    fs_module,
    module_specifiers = ["node:fs", "fs"],
    kind = commonjs,
    default = value(fs_export_value(runtime)?),
);

fn fs_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, FS_EXPORT_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    let payload = runtime.alloc_native_object(FsPayload {
        shared: Arc::new(Mutex::new(FsState {
            next_fd: START_FD,
            files: BTreeMap::new(),
        })),
    });

    for (name, arity, callback) in [
        ("existsSync", 1, fs_exists_sync as _),
        ("readFileSync", 2, fs_read_file_sync as _),
        ("realpathSync", 1, fs_realpath_sync as _),
        ("openSync", 2, fs_open_sync as _),
        ("readSync", 5, fs_read_sync as _),
        ("closeSync", 1, fs_close_sync as _),
        ("mkdirSync", 2, fs_mkdir_sync as _),
        ("rmSync", 2, fs_rm_sync as _),
        ("readdirSync", 1, fs_readdir_sync as _),
        ("statfsSync", 1, fs_statfs_sync as _),
    ] {
        install_method(
            runtime,
            export,
            name,
            arity,
            callback,
            &format!("fs.{name}"),
        )?;
    }
    install_readonly_value(
        runtime,
        export,
        "__otterState",
        RegisterValue::from_object_handle(payload.0),
    )?;

    let constants = runtime.alloc_object();
    install_readonly_value(runtime, constants, "NOATIME", RegisterValue::from_i32(0))?;
    install_readonly_value(
        runtime,
        export,
        "constants",
        RegisterValue::from_object_handle(constants.0),
    )?;

    runtime.install_global_value(FS_EXPORT_SLOT, RegisterValue::from_object_handle(export.0));
    Ok(RegisterValue::from_object_handle(export.0))
}

fn fs_exists_sync(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    Ok(RegisterValue::from_bool(path.exists()))
}

fn fs_read_file_sync(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    let bytes = std::fs::read(&path).map_err(|error| {
        VmNativeCallError::Internal(format!("fs.readFileSync({}): {error}", path.display()).into())
    })?;
    if let Some(encoding) = encoding_arg(runtime, args.get(1).copied())
        && (encoding.eq_ignore_ascii_case("utf8") || encoding.eq_ignore_ascii_case("utf-8"))
    {
        return Ok(string_value(runtime, String::from_utf8_lossy(&bytes)));
    }
    Ok(RegisterValue::from_object_handle(
        alloc_uint8_array(runtime, bytes).0,
    ))
}

fn fs_realpath_sync(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    let resolved = std::fs::canonicalize(&path).map_err(|error| {
        VmNativeCallError::Internal(format!("fs.realpathSync({}): {error}", path.display()).into())
    })?;
    Ok(string_value(runtime, resolved.to_string_lossy()))
}

fn fs_open_sync(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    let file = File::open(&path).map_err(|error| {
        VmNativeCallError::Internal(format!("fs.openSync({}): {error}", path.display()).into())
    })?;
    let state = fs_state_from_this(this, runtime)?;
    let mut state = state
        .lock()
        .map_err(|_| VmNativeCallError::Internal("fs state mutex poisoned".into()))?;
    let fd = state.next_fd;
    state.next_fd += 1;
    state.files.insert(fd, file);
    Ok(RegisterValue::from_i32(fd))
}

fn fs_read_sync(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let fd = args
        .first()
        .and_then(|value| value.as_i32())
        .ok_or_else(|| type_error(runtime, "fs.readSync requires a file descriptor"))?;
    let buffer = args
        .get(1)
        .copied()
        .ok_or_else(|| type_error(runtime, "fs.readSync requires a buffer"))?;
    let offset = args
        .get(2)
        .and_then(|value| value.as_number())
        .unwrap_or(0.0)
        .max(0.0) as usize;
    let length = args
        .get(3)
        .and_then(|value| value.as_number())
        .unwrap_or(0.0)
        .max(0.0) as usize;
    let position = args
        .get(4)
        .and_then(|value| value.as_number())
        .map(|value| value.max(0.0) as u64);

    let state = fs_state_from_this(this, runtime)?;
    let mut state = state
        .lock()
        .map_err(|_| VmNativeCallError::Internal("fs state mutex poisoned".into()))?;
    let file = state
        .files
        .get_mut(&fd)
        .ok_or_else(|| type_error(runtime, "fs.readSync received an unknown file descriptor"))?;
    if let Some(position) = position {
        file.seek(SeekFrom::Start(position)).map_err(|error| {
            VmNativeCallError::Internal(format!("fs.readSync seek failed: {error}").into())
        })?;
    }

    let mut bytes = vec![0u8; length];
    let bytes_read = file.read(&mut bytes).map_err(|error| {
        VmNativeCallError::Internal(format!("fs.readSync failed: {error}").into())
    })?;
    bytes.truncate(bytes_read);
    write_buffer(runtime, buffer, offset, &bytes)?;
    Ok(RegisterValue::from_i32(bytes_read as i32))
}

fn fs_close_sync(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let fd = args
        .first()
        .and_then(|value| value.as_i32())
        .ok_or_else(|| type_error(runtime, "fs.closeSync requires a file descriptor"))?;
    let state = fs_state_from_this(this, runtime)?;
    let mut state = state
        .lock()
        .map_err(|_| VmNativeCallError::Internal("fs state mutex poisoned".into()))?;
    state.files.remove(&fd);
    Ok(RegisterValue::undefined())
}

fn fs_mkdir_sync(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    std::fs::create_dir_all(&path).map_err(|error| {
        VmNativeCallError::Internal(format!("fs.mkdirSync({}): {error}", path.display()).into())
    })?;
    Ok(RegisterValue::undefined())
}

fn fs_rm_sync(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(&path);
    } else {
        let _ = std::fs::remove_file(&path);
    }
    Ok(RegisterValue::undefined())
}

fn fs_readdir_sync(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&path).map_err(|error| {
        VmNativeCallError::Internal(format!("fs.readdirSync({}): {error}", path.display()).into())
    })? {
        let entry = entry.map_err(|error| {
            VmNativeCallError::Internal(format!("fs.readdirSync entry failed: {error}").into())
        })?;
        names.push(string_value(runtime, entry.file_name().to_string_lossy()));
    }
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_array_with_elements(&names).0,
    ))
}

fn fs_statfs_sync(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = path_arg(runtime, args.first().copied())?;
    let object = runtime.alloc_object();
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let c_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| VmNativeCallError::Internal("fs.statfsSync path contains NUL".into()))?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
        if rc != 0 {
            return Err(VmNativeCallError::Internal(
                format!(
                    "fs.statfsSync({}): {}",
                    path.display(),
                    std::io::Error::last_os_error()
                )
                .into(),
            ));
        }
        let stat = unsafe { stat.assume_init() };
        install_readonly_value(
            runtime,
            object,
            "bavail",
            RegisterValue::from_number(stat.f_bavail as f64),
        )
        .map_err(|error| VmNativeCallError::Internal(error.into()))?;
        install_readonly_value(
            runtime,
            object,
            "bsize",
            RegisterValue::from_number(stat.f_bsize as f64),
        )
        .map_err(|error| VmNativeCallError::Internal(error.into()))?;
    }
    #[cfg(not(unix))]
    {
        install_readonly_value(runtime, object, "bavail", RegisterValue::from_i32(0))
            .map_err(|error| VmNativeCallError::Internal(error.into()))?;
        install_readonly_value(runtime, object, "bsize", RegisterValue::from_i32(4096))
            .map_err(|error| VmNativeCallError::Internal(error.into()))?;
    }
    Ok(RegisterValue::from_object_handle(object.0))
}

fn fs_state_from_this(
    this: &RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<Arc<Mutex<FsState>>, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "fs receiver must be an object"))?;
    let property = runtime.intern_property_name("__otterState");
    let value = runtime
        .own_property_value(handle, property)
        .map_err(|_| type_error(runtime, "fs receiver is missing state"))?;
    let Ok(payload) = runtime.native_payload_from_value::<FsPayload>(&value) else {
        return Err(type_error(runtime, "fs receiver has invalid state"));
    };
    Ok(payload.shared.clone())
}

fn path_arg(
    runtime: &mut RuntimeState,
    value: Option<RegisterValue>,
) -> Result<PathBuf, VmNativeCallError> {
    let value = value.ok_or_else(|| type_error(runtime, "path argument is required"))?;
    let rendered = value_to_string(runtime, value);
    let path = PathBuf::from(&rendered);
    if path.is_absolute() {
        Ok(path)
    } else {
        let base = current_node_cwd(runtime)
            .or_else(|| current_process(runtime).map(|(process, _)| process.cwd))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Ok(base.join(path))
    }
}

fn encoding_arg(runtime: &mut RuntimeState, value: Option<RegisterValue>) -> Option<String> {
    let value = value?;
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return None;
    }
    if value.as_object_handle().is_none() {
        return Some(value_to_string(runtime, value));
    }
    let handle = value.as_object_handle().map(ObjectHandle)?;
    own_property(runtime, handle, "encoding").and_then(|encoding| {
        if encoding == RegisterValue::undefined() || encoding == RegisterValue::null() {
            None
        } else {
            Some(value_to_string(runtime, encoding))
        }
    })
}

fn alloc_uint8_array(runtime: &mut RuntimeState, bytes: Vec<u8>) -> ObjectHandle {
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(bytes, None);
    let (_, prototype) = runtime
        .intrinsics()
        .typed_array_constructor_prototype(TypedArrayKind::Uint8);
    let byte_length = runtime
        .objects()
        .array_buffer_byte_length(buffer)
        .unwrap_or_default();
    runtime.objects_mut().alloc_typed_array(
        TypedArrayKind::Uint8,
        buffer,
        0,
        byte_length,
        Some(prototype),
    )
}

fn write_buffer(
    runtime: &mut RuntimeState,
    buffer: RegisterValue,
    offset: usize,
    bytes: &[u8],
) -> Result<(), VmNativeCallError> {
    let handle = buffer
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "fs.readSync buffer must be an object"))?;
    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::TypedArray) => {
            let buffer_handle = runtime
                .objects()
                .typed_array_buffer(handle)
                .map_err(|_| type_error(runtime, "fs.readSync buffer is not a valid TypedArray"))?;
            let byte_offset = runtime
                .objects()
                .typed_array_byte_offset(handle)
                .map_err(|_| type_error(runtime, "fs.readSync buffer offset is invalid"))?;
            let byte_length = runtime
                .objects()
                .typed_array_byte_length(handle)
                .map_err(|_| type_error(runtime, "fs.readSync buffer length is invalid"))?;
            if offset > byte_length {
                return Err(type_error(runtime, "fs.readSync offset is out of range"));
            }
            let writable = runtime
                .objects_mut()
                .array_buffer_or_shared_data_mut(buffer_handle)
                .map_err(|_| {
                    VmNativeCallError::Internal("fs.readSync buffer storage is invalid".into())
                })?;
            let start = byte_offset + offset;
            let end = start + bytes.len().min(byte_length.saturating_sub(offset));
            writable[start..end].copy_from_slice(&bytes[..end - start]);
            Ok(())
        }
        Ok(HeapValueKind::ArrayBuffer) => {
            let writable = runtime
                .objects_mut()
                .array_buffer_or_shared_data_mut(handle)
                .map_err(|_| {
                    VmNativeCallError::Internal("fs.readSync buffer storage is invalid".into())
                })?;
            if offset > writable.len() {
                return Err(type_error(runtime, "fs.readSync offset is out of range"));
            }
            let end = offset + bytes.len().min(writable.len().saturating_sub(offset));
            writable[offset..end].copy_from_slice(&bytes[..end - offset]);
            Ok(())
        }
        _ => Err(type_error(
            runtime,
            "fs.readSync buffer must be an ArrayBuffer or TypedArray",
        )),
    }
}

fn read_global_slot(runtime: &mut RuntimeState, slot: &str) -> Result<RegisterValue, String> {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(slot);
    let value = runtime
        .own_property_value(global, property)
        .map_err(|error| format!("failed to read global slot '{slot}': {error:?}"))?;
    if value == RegisterValue::undefined() {
        return Err(format!("global slot '{slot}' is undefined"));
    }
    Ok(value)
}
