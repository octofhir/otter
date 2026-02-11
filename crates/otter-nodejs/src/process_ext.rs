//! Native `node:process` extension — zero JS shims.
//!
//! Provides the global `process` object with Node.js-compatible API.
//! Replaces `process.rs` (197 lines of serde JSON ops) with native `#[dive]` code.
//!
//! Security model:
//! - `process.chdir` and `process.exit` require subprocess capability.
//! - `process.hrtime` requires explicit `hrtime` capability.
//! - `process.env` reads host values through runtime `env_store` bridge hooks.

use otter_macros::dive;
use otter_vm_core::context::NativeContext;
use otter_vm_core::convert::FromValue;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::intrinsics_impl::reflect::to_property_key;
use otter_vm_core::object::{JsObject, PropertyDescriptor, PropertyKey};
use otter_vm_core::proxy::JsProxy;
use otter_vm_core::proxy_operations::property_key_to_value_pub;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;
use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::Instant;

static START_INSTANT: OnceLock<Instant> = OnceLock::new();

// ---------------------------------------------------------------------------
// OtterExtension implementation
// ---------------------------------------------------------------------------

pub struct NodeProcessExtension;

impl OtterExtension for NodeProcessExtension {
    fn name(&self) -> &str {
        "node_process"
    }

    fn profiles(&self) -> &[Profile] {
        static PROFILES: [Profile; 1] = [Profile::Full];
        &PROFILES
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static SPECIFIERS: [&str; 2] = ["node:process", "process"];
        &SPECIFIERS
    }

    fn install(&self, ctx: &mut RegistrationContext) -> Result<(), otter_vm_core::error::VmError> {
        // Initialize uptime tracking
        START_INSTANT.get_or_init(Instant::now);

        let mm = ctx.mm().clone();
        let process_obj = GcRef::new(JsObject::new(Value::object(ctx.obj_proto()), mm.clone()));

        // --- Static properties ---

        let _ = process_obj.set(
            PropertyKey::string("pid"),
            Value::number(std::process::id() as f64),
        );
        let _ = process_obj.set(PropertyKey::string("platform"), platform_value());
        let _ = process_obj.set(PropertyKey::string("arch"), arch_value());
        let _ = process_obj.set(
            PropertyKey::string("version"),
            Value::string(JsString::intern("v0.1.0-otter")),
        );

        // process.versions
        let versions_obj = GcRef::new(JsObject::new(Value::object(ctx.obj_proto()), mm.clone()));
        let _ = versions_obj.set(
            PropertyKey::string("node"),
            Value::string(JsString::intern("0.1.0")),
        );
        let _ = versions_obj.set(
            PropertyKey::string("otter"),
            Value::string(JsString::intern("0.1.0")),
        );
        let _ = process_obj.set(PropertyKey::string("versions"), Value::object(versions_obj));

        // process.execPath
        let exec_path = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let _ = process_obj.set(
            PropertyKey::string("execPath"),
            Value::string(JsString::new_gc(&exec_path)),
        );

        // process.argv0
        let argv0 = std::env::args().next().unwrap_or_default();
        let _ = process_obj.set(
            PropertyKey::string("argv0"),
            Value::string(JsString::new_gc(&argv0)),
        );

        // process.argv
        let argv_arr = build_argv_array(&mm);
        let _ = process_obj.set(PropertyKey::string("argv"), Value::object(argv_arr));

        // process.execArgv (empty array for now)
        let exec_argv = GcRef::new(JsObject::array(0, mm.clone()));
        let _ = process_obj.set(PropertyKey::string("execArgv"), Value::object(exec_argv));

        // process.env (proxy-backed bridge to runtime env store + capabilities)
        let env_proxy = build_env_proxy(ctx);
        let _ = process_obj.set(PropertyKey::string("env"), Value::proxy(env_proxy));

        // process.exitCode (writable, default 0)
        let _ = process_obj.set(PropertyKey::string("exitCode"), Value::int32(0));

        // process.config — needed by common/index.js (hasCrypto check)
        let config_obj = GcRef::new(JsObject::new(Value::object(ctx.obj_proto()), mm.clone()));
        let config_vars = GcRef::new(JsObject::new(Value::object(ctx.obj_proto()), mm.clone()));
        let _ = config_obj.set(PropertyKey::string("variables"), Value::object(config_vars));
        let _ = config_obj.set(PropertyKey::string("target_defaults"), Value::null());
        let _ = process_obj.set(PropertyKey::string("config"), Value::object(config_obj));

        // process.features — needed by common/index.js
        let features_obj = GcRef::new(JsObject::new(Value::object(ctx.obj_proto()), mm.clone()));
        let _ = features_obj.set(PropertyKey::string("inspector"), Value::boolean(false));
        let _ = features_obj.set(PropertyKey::string("debug"), Value::boolean(false));
        let _ = features_obj.set(PropertyKey::string("tls"), Value::boolean(false));
        let _ = process_obj.set(PropertyKey::string("features"), Value::object(features_obj));

        // process._exiting — needed by common/index.js mustCall
        let _ = process_obj.set(PropertyKey::string("_exiting"), Value::boolean(false));

        // --- Methods ---

        register_process_methods(ctx, &process_obj);

        // process.hrtime.bigint — attach bigint() to the hrtime function object
        if let Some(hrtime_val) = process_obj.get(&PropertyKey::string("hrtime")) {
            if let Some(hrtime_obj) = hrtime_val.as_object() {
                let (bigint_name, bigint_fn, bigint_len) = process_hrtime_bigint_decl();
                let f = bigint_fn.clone();
                let bigint_fn_val = Value::native_function_with_proto(
                    move |this, args, ncx| f(this, args, ncx),
                    mm.clone(),
                    ctx.fn_proto(),
                );
                if let Some(fn_obj) = bigint_fn_val.as_object() {
                    fn_obj.define_property(
                        PropertyKey::string("length"),
                        PropertyDescriptor::function_length(Value::number(bigint_len as f64)),
                    );
                    fn_obj.define_property(
                        PropertyKey::string("name"),
                        PropertyDescriptor::function_length(Value::string(JsString::intern(
                            bigint_name,
                        ))),
                    );
                }
                let _ = hrtime_obj.set(PropertyKey::string("bigint"), bigint_fn_val);
            }
        }

        // Attach to globalThis
        ctx.global_value("process", Value::object(process_obj));

        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        // Return the global process object as the module namespace
        if let Some(process_val) = ctx.global().get(&PropertyKey::string("process")) {
            process_val.as_object()
        } else {
            None
        }
    }
}

/// Create a boxed extension instance for registration.
pub fn node_process_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeProcessExtension)
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn platform_value() -> Value {
    let p = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    Value::string(JsString::intern(p))
}

fn arch_value() -> Value {
    let a = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    Value::string(JsString::intern(a))
}

fn build_argv_array(mm: &std::sync::Arc<otter_vm_core::memory::MemoryManager>) -> GcRef<JsObject> {
    let args: Vec<String> = std::env::args().collect();
    let arr = GcRef::new(JsObject::array(0, mm.clone()));
    for arg in &args {
        arr.array_push(Value::string(JsString::new_gc(arg)));
    }
    arr
}

fn build_env_proxy(ctx: &RegistrationContext) -> GcRef<JsProxy> {
    let target_obj = GcRef::new(JsObject::new(
        Value::object(ctx.obj_proto()),
        ctx.mm().clone(),
    ));
    let handler_obj = GcRef::new(JsObject::new(
        Value::object(ctx.obj_proto()),
        ctx.mm().clone(),
    ));
    register_env_proxy_traps(ctx, &handler_obj);
    JsProxy::new(Value::object(target_obj), Value::object(handler_obj))
}

// ---------------------------------------------------------------------------
// Method registration helper
// ---------------------------------------------------------------------------

/// Register all process methods on the process object using ModuleNamespaceBuilder pattern.
fn register_process_methods(ctx: &RegistrationContext, process_obj: &GcRef<JsObject>) {
    type DeclFn = fn() -> (
        &'static str,
        std::sync::Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        >,
        u32,
    );

    let fns: &[DeclFn] = &[
        process_cwd_decl,
        process_chdir_decl,
        process_exit_decl,
        process_hrtime_decl,
        process_uptime_decl,
        process_memory_usage_decl,
        process_cpu_usage_decl,
        process_next_tick_decl,
        process_umask_decl,
        process_kill_decl,
    ];

    for decl in fns {
        let (name, native_fn, length) = decl();
        let f = native_fn.clone();
        let fn_val = Value::native_function_with_proto(
            move |this, args, ncx| f(this, args, ncx),
            ctx.mm().clone(),
            ctx.fn_proto(),
        );
        if let Some(fn_obj) = fn_val.as_object() {
            fn_obj.define_property(
                PropertyKey::string("length"),
                PropertyDescriptor::function_length(Value::number(length as f64)),
            );
            fn_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
            );
        }
        process_obj.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }
}

fn register_env_proxy_traps(ctx: &RegistrationContext, handler_obj: &GcRef<JsObject>) {
    type DeclFn = fn() -> (
        &'static str,
        std::sync::Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        >,
        u32,
    );

    let traps: &[DeclFn] = &[
        env_proxy_get_decl,
        env_proxy_set_decl,
        env_proxy_has_decl,
        env_proxy_own_keys_decl,
        env_proxy_get_own_property_descriptor_decl,
        env_proxy_delete_property_decl,
    ];

    for decl in traps {
        let (name, native_fn, length) = decl();
        let f = native_fn.clone();
        let fn_val = Value::native_function_with_proto(
            move |this, args, ncx| f(this, args, ncx),
            ctx.mm().clone(),
            ctx.fn_proto(),
        );
        if let Some(fn_obj) = fn_val.as_object() {
            fn_obj.define_property(
                PropertyKey::string("length"),
                PropertyDescriptor::function_length(Value::number(length as f64)),
            );
            fn_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
            );
        }
        handler_obj.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }
}

// ---------------------------------------------------------------------------
// Security bridge
// ---------------------------------------------------------------------------

fn security_err(e: String) -> VmError {
    VmError::type_error(&e)
}

fn call_env_bridge(
    ncx: &mut NativeContext,
    fn_name: &str,
    args: &[Value],
) -> Result<Value, VmError> {
    let global = ncx.global();
    let fn_val = global
        .get(&PropertyKey::string(fn_name))
        .ok_or_else(|| VmError::type_error(format!("Missing global bridge: {fn_name}")))?;

    if !fn_val.is_callable() {
        return Err(VmError::type_error(format!(
            "Global bridge is not callable: {fn_name}"
        )));
    }

    ncx.call_function(&fn_val, Value::undefined(), args)
}

fn env_bridge_has(ncx: &mut NativeContext, key: &Value) -> Result<bool, VmError> {
    Ok(call_env_bridge(ncx, "__env_has", &[key.clone()])?.to_boolean())
}

fn env_bridge_get(ncx: &mut NativeContext, key: &Value) -> Result<Value, VmError> {
    call_env_bridge(ncx, "__env_get", &[key.clone()])
}

fn descriptor_to_object_value(desc: &PropertyDescriptor, ncx: &NativeContext) -> Value {
    let out = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));

    match desc {
        PropertyDescriptor::Data { value, attributes } => {
            let _ = out.set(PropertyKey::string("value"), value.clone());
            let _ = out.set(
                PropertyKey::string("writable"),
                Value::boolean(attributes.writable),
            );
            let _ = out.set(
                PropertyKey::string("enumerable"),
                Value::boolean(attributes.enumerable),
            );
            let _ = out.set(
                PropertyKey::string("configurable"),
                Value::boolean(attributes.configurable),
            );
        }
        PropertyDescriptor::Accessor {
            get,
            set,
            attributes,
        } => {
            if let Some(getter) = get {
                let _ = out.set(PropertyKey::string("get"), getter.clone());
            }
            if let Some(setter) = set {
                let _ = out.set(PropertyKey::string("set"), setter.clone());
            }
            let _ = out.set(
                PropertyKey::string("enumerable"),
                Value::boolean(attributes.enumerable),
            );
            let _ = out.set(
                PropertyKey::string("configurable"),
                Value::boolean(attributes.configurable),
            );
        }
        PropertyDescriptor::Deleted => return Value::undefined(),
    }

    Value::object(out)
}

fn key_identity(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::String(s) => Some(format!("s:{}", s.as_str())),
        PropertyKey::Index(i) => Some(format!("i:{i}")),
        PropertyKey::Symbol(_) => None,
    }
}

// ---------------------------------------------------------------------------
// #[dive] functions
// ---------------------------------------------------------------------------

#[dive(name = "get", length = 3)]
fn env_proxy_get(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let target = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("process.env proxy get: missing target"))?;
    let prop = args.get(1).cloned().unwrap_or_else(Value::undefined);
    let key = to_property_key(&prop);

    if target.get_own_property_descriptor(&key).is_some() {
        return Ok(target.get(&key).unwrap_or(Value::undefined()));
    }

    if let Some(prop_str) = prop.as_string() {
        let prop_val = Value::string(prop_str);
        if env_bridge_has(ncx, &prop_val)? {
            return env_bridge_get(ncx, &prop_val);
        }
    }

    Ok(target.get(&key).unwrap_or(Value::undefined()))
}

#[dive(name = "set", length = 4)]
fn env_proxy_set(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let target = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("process.env proxy set: missing target"))?;
    let prop = args.get(1).cloned().unwrap_or_else(Value::undefined);
    let incoming = args.get(2).cloned().unwrap_or_else(Value::undefined);
    let key = to_property_key(&prop);

    let stored = if prop.as_symbol().is_some() {
        incoming
    } else {
        Value::string(JsString::new_gc(&String::from_value(&incoming)?))
    };
    let _ = target.set(key, stored);
    Ok(Value::boolean(true))
}

#[dive(name = "has", length = 2)]
fn env_proxy_has(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let target = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("process.env proxy has: missing target"))?;
    let prop = args.get(1).cloned().unwrap_or_else(Value::undefined);
    let key = to_property_key(&prop);

    if target.has(&key) {
        return Ok(Value::boolean(true));
    }

    if let Some(prop_str) = prop.as_string() {
        let prop_val = Value::string(prop_str);
        return Ok(Value::boolean(env_bridge_has(ncx, &prop_val)?));
    }

    Ok(Value::boolean(false))
}

#[dive(name = "ownKeys", length = 1)]
fn env_proxy_own_keys(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let target = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("process.env proxy ownKeys: missing target"))?;

    let mut merged: Vec<PropertyKey> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for key in target.own_keys() {
        if let Some(identity) = key_identity(&key) {
            if seen.insert(identity) {
                merged.push(key);
            }
        } else {
            merged.push(key);
        }
    }

    let env_keys = call_env_bridge(ncx, "__env_keys", &[])?;
    if let Some(env_keys_arr) = env_keys.as_object() {
        let len = env_keys_arr.array_length();
        for i in 0..len {
            if let Some(v) = env_keys_arr.get(&PropertyKey::Index(i as u32)) {
                if let Some(s) = v.as_string() {
                    let key = PropertyKey::from_js_string(s);
                    if let Some(identity) = key_identity(&key) {
                        if seen.insert(identity) {
                            merged.push(key);
                        }
                    } else {
                        merged.push(key);
                    }
                }
            }
        }
    }

    let out = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
    for key in &merged {
        out.array_push(property_key_to_value_pub(key));
    }
    Ok(Value::object(out))
}

#[dive(name = "getOwnPropertyDescriptor", length = 2)]
fn env_proxy_get_own_property_descriptor(
    args: &[Value],
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let target = args.first().and_then(|v| v.as_object()).ok_or_else(|| {
        VmError::type_error("process.env proxy getOwnPropertyDescriptor: missing target")
    })?;
    let prop = args.get(1).cloned().unwrap_or_else(Value::undefined);
    let key = to_property_key(&prop);

    if let Some(desc) = target.get_own_property_descriptor(&key) {
        return Ok(descriptor_to_object_value(&desc, ncx));
    }

    if let Some(prop_str) = prop.as_string() {
        let prop_val = Value::string(prop_str);
        if env_bridge_has(ncx, &prop_val)? {
            let value = env_bridge_get(ncx, &prop_val)?;
            let desc = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
            let _ = desc.set(PropertyKey::string("value"), value);
            let _ = desc.set(PropertyKey::string("writable"), Value::boolean(true));
            let _ = desc.set(PropertyKey::string("enumerable"), Value::boolean(true));
            let _ = desc.set(PropertyKey::string("configurable"), Value::boolean(true));
            return Ok(Value::object(desc));
        }
    }

    Ok(Value::undefined())
}

#[dive(name = "deleteProperty", length = 2)]
fn env_proxy_delete_property(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let target = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("process.env proxy deleteProperty: missing target"))?;
    let prop = args.get(1).cloned().unwrap_or_else(Value::undefined);
    let key = to_property_key(&prop);
    Ok(Value::boolean(target.delete(&key)))
}

#[dive(name = "cwd", length = 0)]
fn process_cwd(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let cwd = std::env::current_dir()
        .map_err(|e| VmError::type_error(&format!("Failed to get cwd: {e}")))?;
    Ok(Value::string(JsString::new_gc(&cwd.to_string_lossy())))
}

#[dive(name = "chdir", length = 1)]
fn process_chdir(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let dir = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or_else(|| VmError::type_error("process.chdir requires directory argument"))?;
    let dir_str = dir.as_str();

    crate::security::require_subprocess("process.chdir").map_err(security_err)?;

    std::env::set_current_dir(dir_str).map_err(|e| {
        VmError::type_error(&format!(
            "ENOENT: no such file or directory, chdir '{dir_str}': {e}"
        ))
    })?;
    Ok(Value::undefined())
}

#[dive(name = "exit", length = 1)]
fn process_exit(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let code = args
        .first()
        .and_then(|v| v.as_int32().or_else(|| v.as_number().map(|n| n as i32)))
        .unwrap_or(0);

    crate::security::require_subprocess("process.exit").map_err(security_err)?;

    Err(VmError::type_error(&format!(
        "ProcessExit: code={code}. Host termination is disabled in this runtime."
    )))
}

#[dive(name = "hrtime", length = 1)]
fn process_hrtime(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    crate::security::require_hrtime("process.hrtime").map_err(security_err)?;

    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| VmError::type_error(&format!("Time error: {e}")))?;

    // If previous time provided as [secs, nanos], return difference
    if let Some(prev_arr) = args.first().and_then(|v| v.as_object()) {
        if prev_arr.is_array() {
            let prev_secs = prev_arr
                .get(&PropertyKey::Index(0))
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as u64;
            let prev_nanos = prev_arr
                .get(&PropertyKey::Index(1))
                .and_then(|v| v.as_number())
                .unwrap_or(0.0) as u64;

            let prev_total = prev_secs.saturating_mul(1_000_000_000) + prev_nanos;
            let now_total = now.as_nanos() as u64;
            let diff = now_total.saturating_sub(prev_total);
            let diff_secs = diff / 1_000_000_000;
            let diff_nanos = diff % 1_000_000_000;

            let arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
            arr.array_push(Value::number(diff_secs as f64));
            arr.array_push(Value::number(diff_nanos as f64));
            return Ok(Value::object(arr));
        }
    }

    let arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
    arr.array_push(Value::number(now.as_secs() as f64));
    arr.array_push(Value::number(now.subsec_nanos() as f64));
    Ok(Value::object(arr))
}

/// `process.hrtime.bigint()` — returns nanoseconds as a number (no BigInt support yet).
#[dive(name = "bigint", length = 0)]
fn process_hrtime_bigint(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    crate::security::require_hrtime("process.hrtime.bigint").map_err(security_err)?;

    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| VmError::type_error(&format!("Time error: {e}")))?;

    Ok(Value::number(now.as_nanos() as f64))
}

#[dive(name = "uptime", length = 0)]
fn process_uptime(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let start = START_INSTANT.get_or_init(Instant::now);
    Ok(Value::number(start.elapsed().as_secs_f64().max(0.0)))
}

#[dive(name = "memoryUsage", length = 0)]
fn process_memory_usage(ncx: &mut NativeContext) -> Result<Value, VmError> {
    let mm = ncx.memory_manager();
    let obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    // Best-effort: return stub values; heapUsed could track mm.allocated() later
    let _ = obj.set(PropertyKey::string("rss"), Value::int32(0));
    let _ = obj.set(PropertyKey::string("heapTotal"), Value::int32(0));
    let _ = obj.set(PropertyKey::string("heapUsed"), Value::int32(0));
    let _ = obj.set(PropertyKey::string("external"), Value::int32(0));
    let _ = obj.set(PropertyKey::string("arrayBuffers"), Value::int32(0));
    Ok(Value::object(obj))
}

#[dive(name = "cpuUsage", length = 1)]
fn process_cpu_usage(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    // Stub: return { user: 0, system: 0 } or delta from previous value
    let obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));

    let (prev_user, prev_system) = if let Some(prev) = args.first().and_then(|v| v.as_object()) {
        let u = prev
            .get(&PropertyKey::string("user"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0);
        let s = prev
            .get(&PropertyKey::string("system"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0);
        (u, s)
    } else {
        (0.0, 0.0)
    };

    // Return microseconds since process start (stub)
    let _ = obj.set(PropertyKey::string("user"), Value::number(0.0 - prev_user));
    let _ = obj.set(
        PropertyKey::string("system"),
        Value::number(0.0 - prev_system),
    );
    Ok(Value::object(obj))
}

#[dive(name = "nextTick", length = 1)]
fn process_next_tick(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    let callback = args
        .first()
        .filter(|v| v.is_function())
        .cloned()
        .ok_or_else(|| VmError::type_error("process.nextTick: callback must be a function"))?;

    let extra_args: Vec<Value> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        Vec::new()
    };

    if !ncx.enqueue_next_tick(callback, extra_args) {
        return Err(VmError::type_error(
            "process.nextTick: no nextTick queue available",
        ));
    }

    Ok(Value::undefined())
}

#[dive(name = "umask", length = 1)]
fn process_umask(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    // Stub: always returns 0o022, ignores argument
    let _ = args;
    Ok(Value::int32(0o022))
}

#[dive(name = "kill", length = 2)]
fn process_kill(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    // Stub: no-op
    let _ = args;
    Ok(Value::boolean(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_value() {
        let v = platform_value();
        assert!(v.as_string().is_some());
    }

    #[test]
    fn test_arch_value() {
        let v = arch_value();
        assert!(v.as_string().is_some());
    }
}
