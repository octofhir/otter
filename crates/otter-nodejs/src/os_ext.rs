//! Native `node:os` extension — zero JS shims.
//!
//! All os operations implemented in pure Rust via `#[dive]` + `dive_module!`.
//! Replaces `js/node_os.js` (141 lines) with native code.

use otter_macros::{dive, dive_module};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use std::sync::OnceLock;
use std::time::Instant;

static START_INSTANT: OnceLock<Instant> = OnceLock::new();

dive_module!(
    node_os,
    profiles = [SafeCore, Full],
    module_specifiers = ["node:os", "os"],
    fns = [
        os_platform,
        os_arch,
        os_endianness,
        os_cpus,
        os_freemem,
        os_totalmem,
        os_loadavg,
        os_machine,
        os_version,
        os_homedir,
        os_tmpdir,
        os_hostname,
        os_type,
        os_release,
        os_uptime,
        os_user_info,
        os_network_interfaces,
        os_available_parallelism
    ],
    properties = {
        "EOL" => Value::string(JsString::intern(if cfg!(windows) { "\r\n" } else { "\n" })),
        "devNull" => Value::string(JsString::intern(if cfg!(windows) { "\\\\.\\nul" } else { "/dev/null" })),
    },
);

/// Helper: create a plain object (prototype = null).
fn new_obj(ncx: &mut NativeContext) -> GcRef<JsObject> {
    GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()))
}

/// Helper: create an array.
fn new_arr(ncx: &mut NativeContext, capacity: usize) -> GcRef<JsObject> {
    GcRef::new(JsObject::array(capacity, ncx.memory_manager().clone()))
}

// ---------------------------------------------------------------------------
// #[dive] functions
// ---------------------------------------------------------------------------

#[dive(name = "platform", length = 0)]
fn os_platform(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let p = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    Ok(Value::string(JsString::intern(p)))
}

#[dive(name = "arch", length = 0)]
fn os_arch(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let a = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    Ok(Value::string(JsString::intern(a)))
}

#[dive(name = "endianness", length = 0)]
fn os_endianness(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::string(JsString::intern(
        if cfg!(target_endian = "big") {
            "BE"
        } else {
            "LE"
        },
    )))
}

#[dive(name = "cpus", length = 0)]
fn os_cpus(ncx: &mut NativeContext) -> Result<Value, VmError> {
    let count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let arr = new_arr(ncx, count);

    for _ in 0..count {
        let cpu = new_obj(ncx);
        let _ = cpu.set(
            PropertyKey::string("model"),
            Value::string(JsString::intern("otter-vcpu")),
        );
        let _ = cpu.set(PropertyKey::string("speed"), Value::int32(0));

        let times = new_obj(ncx);
        let _ = times.set(PropertyKey::string("user"), Value::int32(0));
        let _ = times.set(PropertyKey::string("nice"), Value::int32(0));
        let _ = times.set(PropertyKey::string("sys"), Value::int32(0));
        let _ = times.set(PropertyKey::string("idle"), Value::int32(0));
        let _ = times.set(PropertyKey::string("irq"), Value::int32(0));
        let _ = cpu.set(PropertyKey::string("times"), Value::object(times));

        arr.array_push(Value::object(cpu));
    }

    Ok(Value::object(arr))
}

#[dive(name = "freemem", length = 0)]
fn os_freemem(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::int32(0))
}

#[dive(name = "totalmem", length = 0)]
fn os_totalmem(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::int32(0))
}

#[dive(name = "loadavg", length = 0)]
fn os_loadavg(ncx: &mut NativeContext) -> Result<Value, VmError> {
    let arr = new_arr(ncx, 3);
    arr.array_push(Value::int32(0));
    arr.array_push(Value::int32(0));
    arr.array_push(Value::int32(0));
    Ok(Value::object(arr))
}

#[dive(name = "machine", length = 0)]
fn os_machine(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::string(JsString::intern(std::env::consts::ARCH)))
}

#[dive(name = "version", length = 0)]
fn os_version(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::string(JsString::intern("")))
}

#[dive(name = "homedir", length = 0)]
fn os_homedir(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    Ok(Value::string(JsString::new_gc(&home)))
}

#[dive(name = "tmpdir", length = 0)]
fn os_tmpdir(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let tmp = std::env::var("TMPDIR")
        .or_else(|_| std::env::var("TMP"))
        .unwrap_or_else(|_| "/tmp".to_string());
    Ok(Value::string(JsString::new_gc(&tmp)))
}

#[dive(name = "hostname", length = 0)]
fn os_hostname(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    Ok(Value::string(JsString::new_gc(&host)))
}

#[dive(name = "type", length = 0)]
fn os_type(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let t = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        "windows" => "Windows_NT",
        other => other,
    };
    Ok(Value::string(JsString::intern(t)))
}

#[dive(name = "release", length = 0)]
fn os_release(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::string(JsString::intern("")))
}

#[dive(name = "uptime", length = 0)]
fn os_uptime(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let start = START_INSTANT.get_or_init(Instant::now);
    let secs = start.elapsed().as_secs_f64();
    Ok(Value::number(secs.max(0.0)))
}

#[dive(name = "userInfo", length = 0)]
fn os_user_info(ncx: &mut NativeContext) -> Result<Value, VmError> {
    let obj = new_obj(ncx);

    let _ = obj.set(PropertyKey::string("uid"), Value::int32(-1));
    let _ = obj.set(PropertyKey::string("gid"), Value::int32(-1));

    let username = std::env::var("USER").unwrap_or_default();
    let _ = obj.set(
        PropertyKey::string("username"),
        Value::string(JsString::new_gc(&username)),
    );

    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let _ = obj.set(
        PropertyKey::string("homedir"),
        Value::string(JsString::new_gc(&home)),
    );

    let shell = std::env::var("SHELL").unwrap_or_default();
    let _ = obj.set(
        PropertyKey::string("shell"),
        Value::string(JsString::new_gc(&shell)),
    );

    Ok(Value::object(obj))
}

#[dive(name = "networkInterfaces", length = 0)]
fn os_network_interfaces(ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::object(new_obj(ncx)))
}

#[dive(name = "availableParallelism", length = 0)]
fn os_available_parallelism(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    Ok(Value::number(count as f64))
}
