//! Native `node:fs` extension — zero JS shims.
//!
//! All filesystem operations implemented in pure Rust via `#[dive]` + manual
//! `OtterExtension` impl. Replaces `fs.rs` (serde JSON ops) with native code
//! that works directly with `Value` types.
//!
//! Security model:
//! - Read operations require `fs_read` capability.
//! - Write/mutation operations require `fs_write` capability.
//! - Checks are fail-closed at the Rust boundary.

use crate::fs_core::{self, FsMetadata, FsOp, FsOpError, FsOpResult};
use otter_macros::dive;
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyDescriptor, PropertyKey};
use otter_vm_core::promise::{JsPromise, JsPromiseJob, JsPromiseJobKind};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const F_OK: u64 = 0;
const R_OK: u64 = 4;
const W_OK: u64 = 2;
const X_OK: u64 = 1;

// ---------------------------------------------------------------------------
// OtterExtension implementation
// ---------------------------------------------------------------------------

pub struct NodeFsExtension;

impl OtterExtension for NodeFsExtension {
    fn name(&self) -> &str {
        "node_fs"
    }

    fn profiles(&self) -> &[Profile] {
        static PROFILES: [Profile; 1] = [Profile::Full];
        &PROFILES
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static SPECIFIERS: [&str; 4] = ["node:fs", "fs", "node:fs/promises", "fs/promises"];
        &SPECIFIERS
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), otter_vm_core::error::VmError> {
        // fs doesn't install globals
        Ok(())
    }

    fn load_module(
        &self,
        specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let is_promises = specifier == "node:fs/promises" || specifier == "fs/promises";

        if is_promises {
            Some(build_promises_module(ctx))
        } else {
            Some(build_fs_module(ctx))
        }
    }
}

/// Create a boxed extension instance for registration.
pub fn node_fs_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeFsExtension)
}

// ---------------------------------------------------------------------------
// Module builders
// ---------------------------------------------------------------------------

type DeclFn = fn() -> (
    &'static str,
    std::sync::Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    >,
    u32,
);

/// Build the `node:fs` / `fs` module namespace.
fn build_fs_module(ctx: &mut RegistrationContext) -> GcRef<JsObject> {
    let sync_fns: &[DeclFn] = &[
        fs_read_file_sync_decl,
        fs_write_file_sync_decl,
        fs_append_file_sync_decl,
        fs_exists_sync_decl,
        fs_access_sync_decl,
        fs_stat_sync_decl,
        fs_lstat_sync_decl,
        fs_readdir_sync_decl,
        fs_mkdir_sync_decl,
        fs_mkdtemp_sync_decl,
        fs_rmdir_sync_decl,
        fs_rm_sync_decl,
        fs_unlink_sync_decl,
        fs_cp_sync_decl,
        fs_copy_file_sync_decl,
        fs_rename_sync_decl,
        fs_realpath_sync_decl,
        fs_chmod_sync_decl,
        fs_symlink_sync_decl,
        fs_readlink_sync_decl,
    ];

    let mut ns = ctx.module_namespace();
    for decl in sync_fns {
        let (name, native_fn, length) = decl();
        ns = ns.function(name, native_fn, length);
    }

    // constants sub-object
    let constants_obj = build_constants_object(ctx);
    ns = ns.property("constants", Value::object(constants_obj));

    // Also expose F_OK, R_OK, W_OK, X_OK at top level for compat
    ns = ns.property("F_OK", Value::number(F_OK as f64));
    ns = ns.property("R_OK", Value::number(R_OK as f64));
    ns = ns.property("W_OK", Value::number(W_OK as f64));
    ns = ns.property("X_OK", Value::number(X_OK as f64));

    ns.build()
}

/// Build the `node:fs/promises` / `fs/promises` module namespace.
///
/// Methods return Promises that settle through VM-thread job-queue completion.
fn build_promises_module(ctx: &mut RegistrationContext) -> GcRef<JsObject> {
    let async_fns: &[DeclFn] = &[
        fs_read_file_async_decl,
        fs_write_file_async_decl,
        fs_append_file_async_decl,
        fs_stat_async_decl,
        fs_lstat_async_decl,
        fs_readdir_async_decl,
        fs_mkdir_async_decl,
        fs_mkdtemp_async_decl,
        fs_rm_async_decl,
        fs_unlink_async_decl,
        fs_cp_async_decl,
        fs_copy_file_async_decl,
        fs_rename_async_decl,
        fs_realpath_async_decl,
        fs_access_async_decl,
        fs_chmod_async_decl,
        fs_symlink_async_decl,
        fs_readlink_async_decl,
        fs_open_async_decl,
    ];

    let mut ns = ctx.module_namespace();
    for decl in async_fns {
        let (name, native_fn, length) = decl();
        ns = ns.function(name, native_fn, length);
    }

    let constants_obj = build_constants_object(ctx);
    ns = ns.property("constants", Value::object(constants_obj));

    ns.build()
}

/// Build the `constants` sub-object with POSIX file constants.
fn build_constants_object(ctx: &mut RegistrationContext) -> GcRef<JsObject> {
    let obj = ctx.new_object();

    // Access mode constants
    let _ = obj.set(PropertyKey::string("F_OK"), Value::number(F_OK as f64));
    let _ = obj.set(PropertyKey::string("R_OK"), Value::number(R_OK as f64));
    let _ = obj.set(PropertyKey::string("W_OK"), Value::number(W_OK as f64));
    let _ = obj.set(PropertyKey::string("X_OK"), Value::number(X_OK as f64));

    // Open flag constants
    let _ = obj.set(PropertyKey::string("O_RDONLY"), Value::number(0.0));
    let _ = obj.set(PropertyKey::string("O_WRONLY"), Value::number(1.0));
    let _ = obj.set(PropertyKey::string("O_RDWR"), Value::number(2.0));
    let _ = obj.set(PropertyKey::string("O_CREAT"), Value::number(0o100 as f64));
    let _ = obj.set(PropertyKey::string("O_EXCL"), Value::number(0o200 as f64));
    let _ = obj.set(PropertyKey::string("O_TRUNC"), Value::number(0o1000 as f64));
    let _ = obj.set(
        PropertyKey::string("O_APPEND"),
        Value::number(0o2000 as f64),
    );

    // Permission constants
    let _ = obj.set(PropertyKey::string("S_IRUSR"), Value::number(0o400 as f64));
    let _ = obj.set(PropertyKey::string("S_IWUSR"), Value::number(0o200 as f64));
    let _ = obj.set(PropertyKey::string("S_IXUSR"), Value::number(0o100 as f64));
    let _ = obj.set(PropertyKey::string("S_IRGRP"), Value::number(0o040 as f64));
    let _ = obj.set(PropertyKey::string("S_IWGRP"), Value::number(0o020 as f64));
    let _ = obj.set(PropertyKey::string("S_IXGRP"), Value::number(0o010 as f64));
    let _ = obj.set(PropertyKey::string("S_IROTH"), Value::number(0o004 as f64));
    let _ = obj.set(PropertyKey::string("S_IWOTH"), Value::number(0o002 as f64));
    let _ = obj.set(PropertyKey::string("S_IXOTH"), Value::number(0o001 as f64));

    obj
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Extract a required string argument from args.
fn get_required_string(args: &[Value], idx: usize, op: &str) -> Result<String, VmError> {
    args.get(idx)
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .ok_or_else(|| VmError::type_error(&format!("{op} requires argument at index {idx}")))
}

/// Parse encoding from a string value or options object with `encoding` property.
fn parse_encoding(value: Option<&Value>) -> Result<Option<String>, VmError> {
    let Some(value) = value else {
        return Ok(None);
    };

    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }

    // Direct string: "utf8"
    if let Some(s) = value.as_string() {
        return Ok(Some(s.as_str().to_string()));
    }

    // Options object: { encoding: "utf8" }
    if let Some(obj) = value.as_object() {
        if let Some(enc_val) = obj.get(&PropertyKey::string("encoding")) {
            if let Some(s) = enc_val.as_string() {
                return Ok(Some(s.as_str().to_string()));
            }
            if enc_val.is_null() || enc_val.is_undefined() {
                return Ok(None);
            }
        }
        return Ok(None);
    }

    Err(VmError::type_error("Invalid encoding option"))
}

/// Convert a string, array, or Buffer-like value to bytes.
fn data_to_bytes(value: &Value) -> Result<Vec<u8>, VmError> {
    // String value
    if let Some(s) = value.as_string() {
        return Ok(s.as_str().as_bytes().to_vec());
    }

    // Array of byte values
    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            let len = obj
                .get(&PropertyKey::string("length"))
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as usize;
            let mut bytes = Vec::with_capacity(len);
            for i in 0..len {
                let val = obj
                    .get(&PropertyKey::Index(i as u32))
                    .unwrap_or(Value::int32(0));
                let n = val
                    .as_number()
                    .or_else(|| val.as_int32().map(|i| i as f64))
                    .ok_or_else(|| {
                        VmError::type_error(&format!("Byte at index {i} must be a number"))
                    })?;
                if n < 0.0 || n > 255.0 {
                    return Err(VmError::type_error(&format!(
                        "Byte at index {i} must be in range 0..=255"
                    )));
                }
                bytes.push(n as u8);
            }
            return Ok(bytes);
        }
    }

    Err(VmError::type_error(
        "Data must be a string, byte array, or Buffer",
    ))
}

fn current_array_prototype(ncx: &NativeContext) -> Option<GcRef<JsObject>> {
    ncx.global()
        .get(&PropertyKey::string("Array"))
        .and_then(|v| v.as_object())
        .and_then(|array_ctor| array_ctor.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
}

fn create_array(
    mm: &Arc<MemoryManager>,
    array_proto: Option<GcRef<JsObject>>,
    len: usize,
) -> GcRef<JsObject> {
    let arr = GcRef::new(JsObject::array(len, mm.clone()));
    if let Some(proto) = array_proto {
        arr.set_prototype(Value::object(proto));
    }
    arr
}

/// Decode bytes to a Value based on encoding.
fn decode_bytes(
    bytes: &[u8],
    encoding: Option<&str>,
    mm: &Arc<MemoryManager>,
    array_proto: Option<GcRef<JsObject>>,
) -> Result<Value, VmError> {
    match encoding {
        Some("utf8") | Some("utf-8") => Ok(Value::string(JsString::new_gc(
            &String::from_utf8_lossy(bytes),
        ))),
        Some("latin1") | Some("binary") => {
            let s: String = bytes.iter().map(|b| *b as char).collect();
            Ok(Value::string(JsString::new_gc(&s)))
        }
        Some("ascii") => {
            let s: String = bytes.iter().map(|b| (*b & 0x7f) as char).collect();
            Ok(Value::string(JsString::new_gc(&s)))
        }
        Some(enc) => Err(VmError::type_error(&format!("Unknown encoding: {enc}"))),
        // No encoding: return a byte array
        None => {
            let arr = create_array(mm, array_proto, bytes.len());
            for &b in bytes {
                arr.array_push(Value::number(b as f64));
            }
            Ok(Value::array(arr))
        }
    }
}

fn wrap_internal_promise(ncx: &NativeContext, internal: GcRef<JsPromise>) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = obj.set(PropertyKey::string("_internal"), Value::promise(internal));

    if let Some(promise_ctor) = ncx
        .global()
        .get(&PropertyKey::string("Promise"))
        .and_then(|v| v.as_object())
        && let Some(proto) = promise_ctor
            .get(&PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
    {
        if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
            let _ = obj.set(PropertyKey::string("then"), then_fn);
        }
        if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
            let _ = obj.set(PropertyKey::string("catch"), catch_fn);
        }
        if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
            let _ = obj.set(PropertyKey::string("finally"), finally_fn);
        }
        obj.set_prototype(Value::object(proto));
    }

    Value::object(obj)
}

fn parse_open_options(arg: Option<&Value>, op: &str) -> Result<fs_core::FsOpenOptions, VmError> {
    let flag = match arg {
        None => "r".to_string(),
        Some(v) if v.is_null() || v.is_undefined() => "r".to_string(),
        Some(v) => {
            if let Some(s) = v.as_string() {
                s.as_str().to_string()
            } else if let Some(obj) = v.as_object() {
                obj.get(&PropertyKey::string("flags"))
                    .and_then(|v| v.as_string())
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_else(|| "r".to_string())
            } else {
                return Err(VmError::type_error(&format!(
                    "{op} expects flags as string or options object"
                )));
            }
        }
    };

    fs_core::FsOpenOptions::from_flag(&flag)
        .ok_or_else(|| VmError::type_error(&format!("{op} unsupported flags value: {flag}")))
}

fn extract_file_handle_id(this: &Value, op: &str) -> Result<u64, VmError> {
    let Some(this_obj) = this.as_object() else {
        return Err(VmError::type_error(&format!(
            "{op} called on non-FileHandle"
        )));
    };

    let Some(raw_id) = this_obj.get(&PropertyKey::string("__otterFileHandleId")) else {
        return Err(VmError::type_error(&format!(
            "{op} called on invalid FileHandle"
        )));
    };

    if let Some(id) = raw_id.as_number() {
        return Ok(id as u64);
    }
    if let Some(id) = raw_id.as_int32() {
        return Ok(id as u64);
    }

    Err(VmError::type_error(&format!(
        "{op} called on invalid FileHandle"
    )))
}

fn create_file_handle_object(ncx: &mut NativeContext, handle_id: u64) -> Value {
    let mm = ncx.memory_manager().clone();
    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let _ = obj.set(
        PropertyKey::string("__otterFileHandleId"),
        Value::number(handle_id as f64),
    );
    let _ = obj.set(PropertyKey::string("fd"), Value::number(handle_id as f64));

    let close_fn = Value::native_function(
        move |this, _args, close_ncx| {
            let close_id = extract_file_handle_id(this, "FileHandle.close")?;
            spawn_fs_op(
                close_ncx,
                Ok(FsOp::CloseHandle {
                    handle_id: close_id,
                }),
                |result, _callback_ncx| {
                    if !matches!(result, FsOpResult::Unit) {
                        return Err(VmError::type_error(
                            "FileHandle.close: invalid fs op result",
                        ));
                    }
                    Ok(Value::undefined())
                },
            )
        },
        mm,
    );
    obj.define_property(
        PropertyKey::string("close"),
        PropertyDescriptor::builtin_method(close_fn),
    );

    Value::object(obj)
}

/// Format an fs error with an errno-like code.
fn fs_error(op: &str, path: &str, err: io::Error) -> VmError {
    let code = match err.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::IsADirectory => "EISDIR",
        io::ErrorKind::NotADirectory => "ENOTDIR",
        io::ErrorKind::InvalidInput => "EINVAL",
        _ => "EIO",
    };
    VmError::type_error(&format!("{code}: {op} '{path}': {err}"))
}

/// Bridge security errors to VmError.
fn security_err(e: String) -> VmError {
    VmError::type_error(&e)
}

fn construct_error_object(ncx: &mut NativeContext, ctor_name: &str, message: &str) -> Value {
    let msg = Value::string(JsString::new_gc(message));

    if let Some(ctor) = ncx.global().get(&PropertyKey::string(ctor_name))
        && ctor.is_callable()
        && let Ok(value) = ncx.call_function(&ctor, Value::undefined(), &[msg.clone()])
        && value.is_object()
    {
        return value;
    }

    let fallback = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = fallback.set(
        PropertyKey::string("name"),
        Value::string(JsString::new_gc(ctor_name)),
    );
    let _ = fallback.set(PropertyKey::string("message"), msg);
    Value::object(fallback)
}

fn vm_error_to_rejection_value(ncx: &mut NativeContext, err: VmError) -> Value {
    match err {
        VmError::TypeError(msg) => construct_error_object(ncx, "TypeError", &msg),
        VmError::ReferenceError(msg) => construct_error_object(ncx, "ReferenceError", &msg),
        VmError::RangeError(msg) => construct_error_object(ncx, "RangeError", &msg),
        VmError::SyntaxError(msg) => construct_error_object(ncx, "SyntaxError", &msg),
        VmError::InternalError(msg) => construct_error_object(ncx, "Error", &msg),
        VmError::StackOverflow => {
            construct_error_object(ncx, "RangeError", "Maximum call stack size exceeded")
        }
        VmError::OutOfMemory => construct_error_object(ncx, "Error", "OutOfMemory"),
        VmError::Exception(thrown) => thrown.value.clone(),
        VmError::Bytecode(err) => {
            construct_error_object(ncx, "Error", &format!("Bytecode error: {err}"))
        }
        VmError::Interrupted => construct_error_object(ncx, "Error", "Execution interrupted"),
    }
}

fn fs_op_error_to_rejection_value(ncx: &mut NativeContext, err: FsOpError) -> Value {
    let message = err.to_string();
    let value = construct_error_object(ncx, "Error", &message);
    if let Some(obj) = value.as_object() {
        let _ = obj.set(
            PropertyKey::string("code"),
            Value::string(JsString::new_gc(err.code)),
        );
        let _ = obj.set(
            PropertyKey::string("syscall"),
            Value::string(JsString::new_gc(err.syscall)),
        );
        if let Some(path) = err.path {
            let _ = obj.set(
                PropertyKey::string("path"),
                Value::string(JsString::new_gc(&path)),
            );
        }
        if let Some(dest) = err.dest {
            let _ = obj.set(
                PropertyKey::string("dest"),
                Value::string(JsString::new_gc(&dest)),
            );
        }
    }
    value
}

/// Build a Node.js-like Stats object from normalized metadata.
fn build_stat_object_from_core(metadata: &FsMetadata, mm: &Arc<MemoryManager>) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    let _ = obj.set(
        PropertyKey::string("size"),
        Value::number(metadata.size as f64),
    );
    let _ = obj.set(
        PropertyKey::string("mode"),
        Value::number(metadata.mode as f64),
    );
    let _ = obj.set(
        PropertyKey::string("dev"),
        Value::number(metadata.dev as f64),
    );
    let _ = obj.set(
        PropertyKey::string("ino"),
        Value::number(metadata.ino as f64),
    );
    let _ = obj.set(
        PropertyKey::string("nlink"),
        Value::number(metadata.nlink as f64),
    );
    let _ = obj.set(
        PropertyKey::string("uid"),
        Value::number(metadata.uid as f64),
    );
    let _ = obj.set(
        PropertyKey::string("gid"),
        Value::number(metadata.gid as f64),
    );
    let _ = obj.set(
        PropertyKey::string("atimeMs"),
        Value::number(metadata.atime_ms),
    );
    let _ = obj.set(
        PropertyKey::string("mtimeMs"),
        Value::number(metadata.mtime_ms),
    );
    let _ = obj.set(
        PropertyKey::string("ctimeMs"),
        Value::number(metadata.ctime_ms),
    );
    let _ = obj.set(
        PropertyKey::string("birthtimeMs"),
        Value::number(metadata.birthtime_ms),
    );

    let is_file = metadata.is_file;
    let is_dir = metadata.is_dir;
    let is_symlink = metadata.is_symlink;

    let is_file_fn = Value::native_function(
        move |_this, _args, _ncx| Ok(Value::boolean(is_file)),
        mm.clone(),
    );
    obj.define_property(
        PropertyKey::string("isFile"),
        PropertyDescriptor::builtin_method(is_file_fn),
    );

    let is_dir_fn = Value::native_function(
        move |_this, _args, _ncx| Ok(Value::boolean(is_dir)),
        mm.clone(),
    );
    obj.define_property(
        PropertyKey::string("isDirectory"),
        PropertyDescriptor::builtin_method(is_dir_fn),
    );

    let is_symlink_fn = Value::native_function(
        move |_this, _args, _ncx| Ok(Value::boolean(is_symlink)),
        mm.clone(),
    );
    obj.define_property(
        PropertyKey::string("isSymbolicLink"),
        PropertyDescriptor::builtin_method(is_symlink_fn),
    );

    Value::object(obj)
}

// ---------------------------------------------------------------------------
// #[dive] functions — sync fs operations
// ---------------------------------------------------------------------------

#[dive(name = "readFileSync", length = 1)]
fn fs_read_file_sync(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "readFileSync")?;
    let encoding = parse_encoding(args.get(1))?;
    let array_proto = current_array_prototype(ncx);
    let result = fs_core::execute_sync(FsOp::ReadFile { path })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    let bytes = match result {
        FsOpResult::Bytes(bytes) => bytes,
        _ => return Err(VmError::type_error("readFileSync: invalid fs op result")),
    };
    let mm = ncx.memory_manager();
    decode_bytes(&bytes, encoding.as_deref(), mm, array_proto)
}

#[dive(name = "writeFileSync", length = 2)]
fn fs_write_file_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "writeFileSync")?;
    let data = args
        .get(1)
        .ok_or_else(|| VmError::type_error("writeFileSync requires data argument"))?;

    let bytes = data_to_bytes(data)?;
    let append = args
        .get(2)
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&PropertyKey::string("flag")))
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().contains('a'))
        .unwrap_or(false);

    fs_core::execute_sync(FsOp::WriteFile {
        path,
        bytes,
        append,
    })
    .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "appendFileSync", length = 2)]
fn fs_append_file_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "appendFileSync")?;
    let data = args
        .get(1)
        .ok_or_else(|| VmError::type_error("appendFileSync requires data argument"))?;

    let bytes = data_to_bytes(data)?;
    fs_core::execute_sync(FsOp::WriteFile {
        path,
        bytes,
        append: true,
    })
    .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "existsSync", length = 1)]
fn fs_exists_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "existsSync")?;
    crate::security::require_fs_read(&path).map_err(security_err)?;
    Ok(Value::boolean(Path::new(&path).exists()))
}

#[dive(name = "accessSync", length = 1)]
fn fs_access_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "accessSync")?;
    let mode = args
        .get(1)
        .and_then(|v| v.as_number().or_else(|| v.as_int32().map(|i| i as f64)))
        .unwrap_or(F_OK as f64) as u64;
    fs_core::execute_sync(FsOp::Access { path, mode })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "statSync", length = 1)]
fn fs_stat_sync(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "statSync")?;
    let result = fs_core::execute_sync(FsOp::Stat {
        path,
        follow_symlinks: true,
    })
    .map_err(|e| VmError::type_error(&e.to_string()))?;
    let metadata = match result {
        FsOpResult::Metadata(metadata) => metadata,
        _ => return Err(VmError::type_error("statSync: invalid fs op result")),
    };
    let mm = ncx.memory_manager();
    Ok(build_stat_object_from_core(&metadata, mm))
}

#[dive(name = "lstatSync", length = 1)]
fn fs_lstat_sync(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "lstatSync")?;
    let result = fs_core::execute_sync(FsOp::Stat {
        path,
        follow_symlinks: false,
    })
    .map_err(|e| VmError::type_error(&e.to_string()))?;
    let metadata = match result {
        FsOpResult::Metadata(metadata) => metadata,
        _ => return Err(VmError::type_error("lstatSync: invalid fs op result")),
    };
    let mm = ncx.memory_manager();
    Ok(build_stat_object_from_core(&metadata, mm))
}

#[dive(name = "readdirSync", length = 1)]
fn fs_readdir_sync(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "readdirSync")?;
    let array_proto = current_array_prototype(ncx);
    let result = fs_core::execute_sync(FsOp::Readdir { path })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    let names = match result {
        FsOpResult::Strings(names) => names,
        _ => return Err(VmError::type_error("readdirSync: invalid fs op result")),
    };
    let mm = ncx.memory_manager();
    let arr = create_array(mm, array_proto, 0);

    for name in names {
        arr.array_push(Value::string(JsString::new_gc(&name)));
    }

    Ok(Value::array(arr))
}

#[dive(name = "mkdirSync", length = 1)]
fn fs_mkdir_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "mkdirSync")?;
    let recursive = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&PropertyKey::string("recursive")))
        .map(|v| v.to_boolean())
        .unwrap_or(false);

    fs_core::execute_sync(FsOp::Mkdir { path, recursive })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "mkdtempSync", length = 1)]
fn fs_mkdtemp_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let prefix = get_required_string(args, 0, "mkdtempSync")?;
    let result = fs_core::execute_sync(FsOp::Mkdtemp { prefix })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    let path = match result {
        FsOpResult::String(path) => path,
        _ => return Err(VmError::type_error("mkdtempSync: invalid fs op result")),
    };
    Ok(Value::string(JsString::new_gc(&path)))
}

#[dive(name = "rmdirSync", length = 1)]
fn fs_rmdir_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "rmdirSync")?;
    crate::security::require_fs_write(&path).map_err(security_err)?;

    let recursive = args
        .get(1)
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(&PropertyKey::string("recursive")))
        .map(|v| v.to_boolean())
        .unwrap_or(false);

    if recursive {
        std::fs::remove_dir_all(&path).map_err(|e| fs_error("rmdirSync", &path, e))?;
    } else {
        std::fs::remove_dir(&path).map_err(|e| fs_error("rmdirSync", &path, e))?;
    }

    Ok(Value::undefined())
}

#[dive(name = "rmSync", length = 1)]
fn fs_rm_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "rmSync")?;
    let opts = args.get(1).and_then(|v| v.as_object());
    let recursive = opts
        .as_ref()
        .and_then(|obj| obj.get(&PropertyKey::string("recursive")))
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let force = opts
        .as_ref()
        .and_then(|obj| obj.get(&PropertyKey::string("force")))
        .map(|v| v.to_boolean())
        .unwrap_or(false);

    fs_core::execute_sync(FsOp::Rm {
        path,
        recursive,
        force,
    })
    .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "unlinkSync", length = 1)]
fn fs_unlink_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "unlinkSync")?;
    fs_core::execute_sync(FsOp::Unlink { path })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "cpSync", length = 2)]
fn fs_cp_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let src = get_required_string(args, 0, "cpSync")?;
    let dst = get_required_string(args, 1, "cpSync")?;
    let opts = args.get(2).and_then(|v| v.as_object());
    let recursive = opts
        .as_ref()
        .and_then(|obj| obj.get(&PropertyKey::string("recursive")))
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let force = opts
        .as_ref()
        .and_then(|obj| obj.get(&PropertyKey::string("force")))
        .map(|v| v.to_boolean())
        .unwrap_or(false);

    fs_core::execute_sync(FsOp::Cp {
        src,
        dst,
        recursive,
        force,
    })
    .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "copyFileSync", length = 2)]
fn fs_copy_file_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let src = get_required_string(args, 0, "copyFileSync")?;
    let dst = get_required_string(args, 1, "copyFileSync")?;

    fs_core::execute_sync(FsOp::CopyFile { src, dst })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "renameSync", length = 2)]
fn fs_rename_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let from = get_required_string(args, 0, "renameSync")?;
    let to = get_required_string(args, 1, "renameSync")?;
    fs_core::execute_sync(FsOp::Rename { from, to })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "realpathSync", length = 1)]
fn fs_realpath_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "realpathSync")?;
    let result = fs_core::execute_sync(FsOp::Realpath { path })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    let canonical = match result {
        FsOpResult::String(path) => path,
        _ => return Err(VmError::type_error("realpathSync: invalid fs op result")),
    };
    Ok(Value::string(JsString::new_gc(&canonical)))
}

#[dive(name = "chmodSync", length = 2)]
fn fs_chmod_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "chmodSync")?;
    let mode =
        args.get(1)
            .and_then(|v| v.as_number().or_else(|| v.as_int32().map(|i| i as f64)))
            .ok_or_else(|| VmError::type_error("chmodSync requires mode argument"))? as u32;
    fs_core::execute_sync(FsOp::Chmod { path, mode })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "symlinkSync", length = 2)]
fn fs_symlink_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let target = get_required_string(args, 0, "symlinkSync")?;
    let link_path = get_required_string(args, 1, "symlinkSync")?;
    fs_core::execute_sync(FsOp::Symlink { target, link_path })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    Ok(Value::undefined())
}

#[dive(name = "readlinkSync", length = 1)]
fn fs_readlink_sync(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let path = get_required_string(args, 0, "readlinkSync")?;
    let result = fs_core::execute_sync(FsOp::Readlink { path })
        .map_err(|e| VmError::type_error(&e.to_string()))?;
    let target = match result {
        FsOpResult::String(path) => path,
        _ => return Err(VmError::type_error("readlinkSync: invalid fs op result")),
    };
    Ok(Value::string(JsString::new_gc(&target)))
}

// ---------------------------------------------------------------------------
// #[dive] functions — promise-returning async fs operations (tokio::fs)
// ---------------------------------------------------------------------------

/// Decrements `pending_async_ops` when dropped, including panic/unwind paths.
struct PendingOpGuard(Option<Arc<AtomicU64>>);

impl PendingOpGuard {
    fn new(counter: Option<Arc<AtomicU64>>) -> Self {
        if let Some(counter) = &counter {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        Self(counter)
    }
}

impl Drop for PendingOpGuard {
    fn drop(&mut self) {
        if let Some(counter) = &self.0 {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

enum FsAsyncSetupError {
    Vm(VmError),
    Fs(FsOpError),
}

impl From<VmError> for FsAsyncSetupError {
    fn from(value: VmError) -> Self {
        Self::Vm(value)
    }
}

impl From<FsOpError> for FsAsyncSetupError {
    fn from(value: FsOpError) -> Self {
        Self::Fs(value)
    }
}

/// Create a Promise backed by a tokio task.
///
/// `setup` runs synchronously on the VM thread — validates args and performs
/// capability checks.
///
/// Worker threads execute only plain Rust fs operations. JS values and
/// Promise settle operations are marshalled back to the VM thread through
/// JS job queue callback dispatch.
fn spawn_fs_op<C>(
    ncx: &mut NativeContext,
    setup: Result<FsOp, FsAsyncSetupError>,
    convert: C,
) -> Result<Value, VmError>
where
    C: Fn(FsOpResult, &mut NativeContext) -> Result<Value, VmError> + Send + Sync + 'static,
{
    let mm = ncx.memory_manager().clone();
    let js_queue = ncx
        .js_job_queue()
        .ok_or_else(|| VmError::type_error("No JS job queue available for async fs operation"))?;
    let pending_ops = ncx.pending_async_ops();

    let js_queue_for_resolvers = Arc::clone(&js_queue);
    let resolvers = JsPromise::with_resolvers(mm.clone(), move |job, args| {
        js_queue_for_resolvers.enqueue(job, args);
    });

    // Validate args synchronously — reject promise on failure.
    let op = match setup {
        Ok(op) => op,
        Err(FsAsyncSetupError::Fs(err)) => {
            (resolvers.reject)(fs_op_error_to_rejection_value(ncx, err));
            return Ok(wrap_internal_promise(ncx, resolvers.promise.clone()));
        }
        Err(FsAsyncSetupError::Vm(err)) => {
            (resolvers.reject)(vm_error_to_rejection_value(ncx, err));
            return Ok(wrap_internal_promise(ncx, resolvers.promise.clone()));
        }
    };

    // Check if we can get a tokio handle.
    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(_) => {
            (resolvers.reject)(construct_error_object(
                ncx,
                "Error",
                "No async runtime available for fs/promises operation",
            ));
            return Ok(wrap_internal_promise(ncx, resolvers.promise.clone()));
        }
    };

    let resolve = resolvers.resolve.clone();
    let reject = resolvers.reject.clone();
    let pending_ops_clone = pending_ops.clone();
    let converter = Arc::new(convert);
    let completion_slot: Arc<Mutex<Option<Result<FsOpResult, FsOpError>>>> =
        Arc::new(Mutex::new(None));

    let completion_slot_for_callback = Arc::clone(&completion_slot);
    let converter_for_callback = Arc::clone(&converter);
    let resolve_for_callback = resolve.clone();
    let reject_for_callback = reject.clone();
    let completion_callback = Value::native_function(
        move |_this, _args, callback_ncx| {
            let outcome = match completion_slot_for_callback.lock() {
                Ok(mut guard) => guard.take(),
                Err(_) => None,
            };
            let Some(outcome) = outcome else {
                return Ok(Value::undefined());
            };

            match outcome {
                Ok(result) => match converter_for_callback(result, callback_ncx) {
                    Ok(value) => resolve_for_callback(value),
                    Err(err) => reject_for_callback(vm_error_to_rejection_value(callback_ncx, err)),
                },
                Err(err) => reject_for_callback(fs_op_error_to_rejection_value(callback_ncx, err)),
            }

            Ok(Value::undefined())
        },
        mm.clone(),
    );

    let completion_slot_for_worker = Arc::clone(&completion_slot);
    let js_queue_for_worker = Arc::clone(&js_queue);
    let completion_callback_for_worker = completion_callback.clone();

    handle.spawn(async move {
        let _pending_guard = PendingOpGuard::new(pending_ops_clone);
        let outcome = fs_core::execute_async_unchecked(op).await;
        if let Ok(mut guard) = completion_slot_for_worker.lock() {
            *guard = Some(outcome);
        }

        js_queue_for_worker.enqueue(
            JsPromiseJob {
                kind: JsPromiseJobKind::Fulfill,
                callback: completion_callback_for_worker,
                this_arg: Value::undefined(),
                result_promise: None,
            },
            Vec::new(),
        );
    });

    Ok(wrap_internal_promise(ncx, resolvers.promise))
}

#[dive(name = "readFile", length = 1)]
fn fs_read_file_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<(FsOp, Option<String>), FsAsyncSetupError> {
        let path = get_required_string(args, 0, "readFile")?;
        let encoding = parse_encoding(args.get(1))?;
        let op = FsOp::ReadFile { path };
        fs_core::precheck_capabilities(&op)?;
        Ok((op, encoding))
    })();
    let (setup_op, encoding) = match setup {
        Ok((op, encoding)) => (Ok(op), encoding),
        Err(e) => (Err(e), None),
    };

    spawn_fs_op(ncx, setup_op, move |result, callback_ncx| {
        let bytes = match result {
            FsOpResult::Bytes(bytes) => bytes,
            _ => return Err(VmError::type_error("readFile: invalid fs op result")),
        };
        let mm = callback_ncx.memory_manager();
        let array_proto = current_array_prototype(callback_ncx);
        decode_bytes(&bytes, encoding.as_deref(), mm, array_proto)
    })
}

#[dive(name = "writeFile", length = 2)]
fn fs_write_file_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "writeFile")?;
        let data = args
            .get(1)
            .ok_or_else(|| VmError::type_error("writeFile requires data argument"))?;
        let bytes = data_to_bytes(data)?;
        let append = args
            .get(2)
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get(&PropertyKey::string("flag")))
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().contains('a'))
            .unwrap_or(false);
        let op = FsOp::WriteFile {
            path,
            bytes,
            append,
        };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("writeFile: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "appendFile", length = 2)]
fn fs_append_file_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "appendFile")?;
        let data = args
            .get(1)
            .ok_or_else(|| VmError::type_error("appendFile requires data argument"))?;
        let bytes = data_to_bytes(data)?;
        let op = FsOp::WriteFile {
            path,
            bytes,
            append: true,
        };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("appendFile: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "stat", length = 1)]
fn fs_stat_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "stat")?;
        let op = FsOp::Stat {
            path,
            follow_symlinks: true,
        };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, callback_ncx| {
        let metadata = match result {
            FsOpResult::Metadata(metadata) => metadata,
            _ => return Err(VmError::type_error("stat: invalid fs op result")),
        };
        Ok(build_stat_object_from_core(
            &metadata,
            callback_ncx.memory_manager(),
        ))
    })
}

#[dive(name = "lstat", length = 1)]
fn fs_lstat_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "lstat")?;
        let op = FsOp::Stat {
            path,
            follow_symlinks: false,
        };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, callback_ncx| {
        let metadata = match result {
            FsOpResult::Metadata(metadata) => metadata,
            _ => return Err(VmError::type_error("lstat: invalid fs op result")),
        };
        Ok(build_stat_object_from_core(
            &metadata,
            callback_ncx.memory_manager(),
        ))
    })
}

#[dive(name = "readdir", length = 1)]
fn fs_readdir_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "readdir")?;
        let op = FsOp::Readdir { path };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, callback_ncx| {
        let names = match result {
            FsOpResult::Strings(names) => names,
            _ => return Err(VmError::type_error("readdir: invalid fs op result")),
        };
        let mm = callback_ncx.memory_manager();
        let array_proto = current_array_prototype(callback_ncx);
        let arr = create_array(mm, array_proto, 0);
        for name in names {
            arr.array_push(Value::string(JsString::new_gc(&name)));
        }
        Ok(Value::array(arr))
    })
}

#[dive(name = "mkdir", length = 1)]
fn fs_mkdir_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "mkdir")?;
        let recursive = args
            .get(1)
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get(&PropertyKey::string("recursive")))
            .map(|v| v.to_boolean())
            .unwrap_or(false);
        let op = FsOp::Mkdir { path, recursive };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("mkdir: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "mkdtemp", length = 1)]
fn fs_mkdtemp_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let prefix = get_required_string(args, 0, "mkdtemp")?;
        let op = FsOp::Mkdtemp { prefix };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        let path = match result {
            FsOpResult::String(path) => path,
            _ => return Err(VmError::type_error("mkdtemp: invalid fs op result")),
        };
        Ok(Value::string(JsString::new_gc(&path)))
    })
}

#[dive(name = "rm", length = 1)]
fn fs_rm_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "rm")?;
        let opts = args.get(1).and_then(|v| v.as_object());
        let recursive = opts
            .as_ref()
            .and_then(|obj| obj.get(&PropertyKey::string("recursive")))
            .map(|v| v.to_boolean())
            .unwrap_or(false);
        let force = opts
            .as_ref()
            .and_then(|obj| obj.get(&PropertyKey::string("force")))
            .map(|v| v.to_boolean())
            .unwrap_or(false);
        let op = FsOp::Rm {
            path,
            recursive,
            force,
        };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("rm: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "unlink", length = 1)]
fn fs_unlink_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "unlink")?;
        let op = FsOp::Unlink { path };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("unlink: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "cp", length = 2)]
fn fs_cp_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let src = get_required_string(args, 0, "cp")?;
        let dst = get_required_string(args, 1, "cp")?;
        let opts = args.get(2).and_then(|v| v.as_object());
        let recursive = opts
            .as_ref()
            .and_then(|obj| obj.get(&PropertyKey::string("recursive")))
            .map(|v| v.to_boolean())
            .unwrap_or(false);
        let force = opts
            .as_ref()
            .and_then(|obj| obj.get(&PropertyKey::string("force")))
            .map(|v| v.to_boolean())
            .unwrap_or(false);

        let op = FsOp::Cp {
            src,
            dst,
            recursive,
            force,
        };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("cp: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "copyFile", length = 2)]
fn fs_copy_file_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let src = get_required_string(args, 0, "copyFile")?;
        let dst = get_required_string(args, 1, "copyFile")?;
        let op = FsOp::CopyFile { src, dst };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("copyFile: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "rename", length = 2)]
fn fs_rename_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let from = get_required_string(args, 0, "rename")?;
        let to = get_required_string(args, 1, "rename")?;
        let op = FsOp::Rename { from, to };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("rename: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "realpath", length = 1)]
fn fs_realpath_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "realpath")?;
        let op = FsOp::Realpath { path };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        let canonical = match result {
            FsOpResult::String(path) => path,
            _ => return Err(VmError::type_error("realpath: invalid fs op result")),
        };
        Ok(Value::string(JsString::new_gc(&canonical)))
    })
}

#[dive(name = "access", length = 1)]
fn fs_access_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "access")?;
        let mode = args
            .get(1)
            .and_then(|v| v.as_number().or_else(|| v.as_int32().map(|i| i as f64)))
            .map(|n| n as u64)
            .unwrap_or(F_OK);
        let op = FsOp::Access { path, mode };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("access: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "chmod", length = 2)]
fn fs_chmod_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "chmod")?;
        let mode = args
            .get(1)
            .and_then(|v| v.as_number().or_else(|| v.as_int32().map(|i| i as f64)))
            .ok_or_else(|| VmError::type_error("chmod requires mode argument"))?
            as u32;
        let op = FsOp::Chmod { path, mode };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("chmod: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "symlink", length = 2)]
fn fs_symlink_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let target = get_required_string(args, 0, "symlink")?;
        let link_path = get_required_string(args, 1, "symlink")?;
        let op = FsOp::Symlink { target, link_path };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        if !matches!(result, FsOpResult::Unit) {
            return Err(VmError::type_error("symlink: invalid fs op result"));
        }
        Ok(Value::undefined())
    })
}

#[dive(name = "readlink", length = 1)]
fn fs_readlink_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "readlink")?;
        let op = FsOp::Readlink { path };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, _callback_ncx| {
        let target = match result {
            FsOpResult::String(path) => path,
            _ => return Err(VmError::type_error("readlink: invalid fs op result")),
        };
        Ok(Value::string(JsString::new_gc(&target)))
    })
}

#[dive(name = "open", length = 2)]
fn fs_open_async(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let setup = (|| -> Result<FsOp, FsAsyncSetupError> {
        let path = get_required_string(args, 0, "open")?;
        let flags = parse_open_options(args.get(1), "open")?;
        let op = FsOp::Open { path, flags };
        fs_core::precheck_capabilities(&op)?;
        Ok(op)
    })();

    spawn_fs_op(ncx, setup, |result, callback_ncx| {
        let handle_id = match result {
            FsOpResult::FileHandle(handle_id) => handle_id,
            _ => return Err(VmError::type_error("open: invalid fs op result")),
        };
        Ok(create_file_handle_object(callback_ncx, handle_id))
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_encoding_string() {
        let val = Value::string(JsString::intern("utf8"));
        let result = parse_encoding(Some(&val)).unwrap();
        assert_eq!(result, Some("utf8".to_string()));
    }

    #[test]
    fn test_parse_encoding_none() {
        let result = parse_encoding(None).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_encoding_null() {
        let result = parse_encoding(Some(&Value::null())).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_encoding_undefined() {
        let result = parse_encoding(Some(&Value::undefined())).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_data_to_bytes_string() {
        let val = Value::string(JsString::new_gc("hello"));
        let bytes = data_to_bytes(&val).unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn test_get_required_string_missing() {
        let args: Vec<Value> = vec![];
        let err = get_required_string(&args, 0, "test").unwrap_err();
        assert!(err.to_string().contains("requires argument"));
    }

    #[test]
    fn test_fs_error_format() {
        let err = fs_error(
            "readFileSync",
            "/tmp/no-exist",
            io::Error::new(io::ErrorKind::NotFound, "not found"),
        );
        let msg = err.to_string();
        assert!(msg.contains("ENOENT"));
        assert!(msg.contains("readFileSync"));
    }
}
