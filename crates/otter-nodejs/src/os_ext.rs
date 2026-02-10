//! Native `node:os` extension — zero JS shims.
//!
//! All os operations implemented in pure Rust via `#[dive]` + `dive_module!`.
//! Uses `sysinfo` crate for real system data.

use otter_macros::{dive, dive_module};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};

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
fn new_arr(ncx: &mut NativeContext, _capacity: usize) -> GcRef<JsObject> {
    GcRef::new(JsObject::array(0, ncx.memory_manager().clone()))
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
    let mut sys =
        System::new_with_specifics(RefreshKind::nothing().with_cpu(CpuRefreshKind::everything()));
    // Need a second refresh to get meaningful CPU usage values
    std::thread::sleep(std::time::Duration::from_millis(100));
    sys.refresh_cpu_all();

    let cpus = sys.cpus();
    let arr = new_arr(ncx, cpus.len());

    for cpu in cpus {
        let cpu_obj = new_obj(ncx);
        let _ = cpu_obj.set(
            PropertyKey::string("model"),
            Value::string(JsString::new_gc(cpu.brand())),
        );
        let _ = cpu_obj.set(
            PropertyKey::string("speed"),
            Value::number(cpu.frequency() as f64),
        );

        let times = new_obj(ncx);
        // sysinfo doesn't provide per-CPU user/sys/idle breakdown,
        // but we can give total usage as user and rest as idle.
        // Node reports these in milliseconds.
        let usage = cpu.cpu_usage() as f64;
        let _ = times.set(PropertyKey::string("user"), Value::number(usage * 10.0));
        let _ = times.set(PropertyKey::string("nice"), Value::int32(0));
        let _ = times.set(PropertyKey::string("sys"), Value::int32(0));
        let _ = times.set(
            PropertyKey::string("idle"),
            Value::number((100.0 - usage) * 10.0),
        );
        let _ = times.set(PropertyKey::string("irq"), Value::int32(0));
        let _ = cpu_obj.set(PropertyKey::string("times"), Value::object(times));

        arr.array_push(Value::object(cpu_obj));
    }

    Ok(Value::object(arr))
}

#[dive(name = "freemem", length = 0)]
fn os_freemem(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
    );
    sys.refresh_memory();
    Ok(Value::number(sys.available_memory() as f64))
}

#[dive(name = "totalmem", length = 0)]
fn os_totalmem(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
    );
    sys.refresh_memory();
    Ok(Value::number(sys.total_memory() as f64))
}

#[cfg(unix)]
#[dive(name = "loadavg", length = 0)]
fn os_loadavg(ncx: &mut NativeContext) -> Result<Value, VmError> {
    let arr = new_arr(ncx, 3);
    let load = System::load_average();
    arr.array_push(Value::number(load.one));
    arr.array_push(Value::number(load.five));
    arr.array_push(Value::number(load.fifteen));
    Ok(Value::object(arr))
}

#[cfg(not(unix))]
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
    let ver = System::os_version().unwrap_or_default();
    Ok(Value::string(JsString::new_gc(&ver)))
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
    let host = System::host_name().unwrap_or_else(|| "localhost".to_string());
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
    let ver = System::kernel_version().unwrap_or_default();
    Ok(Value::string(JsString::new_gc(&ver)))
}

#[dive(name = "uptime", length = 0)]
fn os_uptime(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    Ok(Value::number(System::uptime() as f64))
}

#[dive(name = "userInfo", length = 0)]
fn os_user_info(ncx: &mut NativeContext) -> Result<Value, VmError> {
    let obj = new_obj(ncx);

    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() } as i32;
        let gid = unsafe { libc::getgid() } as i32;
        let _ = obj.set(PropertyKey::string("uid"), Value::int32(uid));
        let _ = obj.set(PropertyKey::string("gid"), Value::int32(gid));
    }
    #[cfg(not(unix))]
    {
        let _ = obj.set(PropertyKey::string("uid"), Value::int32(-1));
        let _ = obj.set(PropertyKey::string("gid"), Value::int32(-1));
    }

    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
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
    let networks = Networks::new_with_refreshed_list();
    let result = new_obj(ncx);

    for (name, data) in &networks {
        let iface_arr = new_arr(ncx, 1);

        let entry = new_obj(ncx);
        // MAC address
        let mac = data.mac_address().to_string();
        let _ = entry.set(
            PropertyKey::string("mac"),
            Value::string(JsString::new_gc(&mac)),
        );

        // Network stats — sysinfo doesn't expose IP addresses directly,
        // but we can provide what's available
        let _ = entry.set(
            PropertyKey::string("internal"),
            Value::boolean(name == "lo" || name == "lo0" || name.starts_with("loopback")),
        );

        iface_arr.array_push(Value::object(entry));
        let _ = result.set(PropertyKey::string(name), Value::object(iface_arr));
    }

    Ok(Value::object(result))
}

#[dive(name = "availableParallelism", length = 0)]
fn os_available_parallelism(_ncx: &mut NativeContext) -> Result<Value, VmError> {
    let count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    Ok(Value::number(count as f64))
}
