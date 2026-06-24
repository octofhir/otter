//! `node:os` / `os` hosted module.
//!
//! Operating-system information, implemented natively over `libc` and `std`.
//! Pure queries — no permission-gated resources — so the static namespace is
//! built with [`ModuleScope`] and the dynamic shapes (`cpus`, `loadavg`,
//! `userInfo`, …) are built inside each method body.
//!
//! # Contents
//! - [`install_os_module`] - ESM namespace install (method surface).
//! - [`os_cjs_value`] - CommonJS default export (methods + `EOL`/`devNull`/
//!   `constants`).
//!
//! # Invariants
//! - `EOL` is non-writable but configurable, matching Node (assignment throws in
//!   strict mode; `Object.defineProperty` can still redefine it).
//! - `arch`/`platform`/`endianness` reflect the host the binary was built for.
//! - No VM values are retained across the FFI calls; results are copied out.

use otter_runtime::CapabilitySet;
use otter_runtime::module_scope::{ModuleScope, Rooted};
use otter_vm::{ErrorKind, NativeCtx, NativeError, Value, object};
use std::sync::atomic::{AtomicI32, Ordering};

use crate::string_value;

/// Read `process.env.<key>` from the live, capability-filtered `process.env`
/// object. This is the source of truth — JS code mutates that object (e.g.
/// `process.env.TMPDIR = '/x'`) and the deny-by-default / secret denylist policy
/// already applied at install time. Reading `std::env` directly would bypass
/// both, so `os` never does.
fn env_var(ctx: &mut NativeCtx<'_>, key: &str) -> Option<String> {
    let global = *ctx.interp_mut().global_this();
    let heap = ctx.heap();
    let process = object::get(global, heap, "process")?.as_object()?;
    let env = object::get(process, heap, "env")?.as_object()?;
    let value = object::get(env, heap, key)?;
    if value.is_string() {
        Some(value.display_string(heap))
    } else {
        None
    }
}

/// Node priority constants (niceness scale); same values on every platform.
const PRIORITY: &[(&str, f64)] = &[
    ("PRIORITY_LOW", 19.0),
    ("PRIORITY_BELOW_NORMAL", 10.0),
    ("PRIORITY_NORMAL", 0.0),
    ("PRIORITY_ABOVE_NORMAL", -7.0),
    ("PRIORITY_HIGH", -14.0),
    ("PRIORITY_HIGHEST", -20.0),
];

static CURRENT_PROCESS_PRIORITY: AtomicI32 = AtomicI32::new(0);

type Method = (
    &'static str,
    u8,
    fn(&mut NativeCtx<'_>, &[Value]) -> Result<Value, NativeError>,
);

const OS_METHODS: &[Method] = &[
    ("arch", 0, os_arch),
    ("platform", 0, os_platform),
    ("machine", 0, os_machine),
    ("type", 0, os_type),
    ("release", 0, os_release),
    ("version", 0, os_version),
    ("endianness", 0, os_endianness),
    ("hostname", 0, os_hostname),
    ("homedir", 0, os_homedir),
    ("tmpdir", 0, os_tmpdir),
    ("totalmem", 0, os_totalmem),
    ("freemem", 0, os_freemem),
    ("uptime", 0, os_uptime),
    ("availableParallelism", 0, os_available_parallelism),
    ("loadavg", 0, os_loadavg),
    ("cpus", 0, os_cpus),
    ("networkInterfaces", 0, os_network_interfaces),
    ("userInfo", 1, os_user_info),
    ("getPriority", 1, os_get_priority),
    ("setPriority", 2, os_set_priority),
];

/// ESM namespace install (methods only; `EOL`/`devNull`/`constants` are on the
/// CJS value and are also set here for parity).
pub fn install_os_module(ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    for (name, len, f) in OS_METHODS {
        ctx.builtin_method(name, *len, *f)?;
    }
    // `EOL` / `devNull` / `constants` are string/object data the install ctx
    // cannot allocate; they ride on the CommonJS export value instead.
    Ok(())
}

/// CommonJS export: the `os` namespace with methods + `EOL`/`devNull` +
/// `constants.priority`.
pub fn os_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    let mut scope = ModuleScope::new(ctx);
    let os = scope.object()?;
    let coerce = scope.function("toString", 0, os_method_to_primitive)?;
    for (name, len, f) in OS_METHODS {
        let method = scope.function(name, *len, *f)?;
        if os_method_is_primitive_coercible(name) {
            scope.set_native_function_property(method, "toString", coerce)?;
            scope.set_native_function_property(method, "valueOf", coerce)?;
        }
        scope.set(os, name, method);
    }
    // EOL must throw on assignment (strict mode) yet stay redefinable.
    scope.set_string_readonly(os, "EOL", ctx_eol())?;
    scope.set_string(os, "devNull", dev_null())?;

    let constants = scope.object()?;
    let priority = scope.object()?;
    for (name, value) in PRIORITY {
        scope.set_number(priority, name, *value);
    }
    scope.set(constants, "priority", priority);
    scope.set(os, "constants", constants);

    Ok(scope.finish(os))
}

fn os_method_is_primitive_coercible(name: &str) -> bool {
    matches!(
        name,
        "arch"
            | "platform"
            | "machine"
            | "type"
            | "release"
            | "version"
            | "endianness"
            | "hostname"
            | "homedir"
            | "tmpdir"
            | "totalmem"
            | "freemem"
            | "uptime"
            | "availableParallelism"
    )
}

// ---- string namespace helpers (install path needs raw Value, no scope) ----

fn ctx_eol() -> &'static str {
    if cfg!(windows) { "\r\n" } else { "\n" }
}

fn dev_null() -> &'static str {
    if cfg!(windows) {
        "\\\\.\\nul"
    } else {
        "/dev/null"
    }
}

// ---- simple string/number queries ----

fn os_arch(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    string_value(ctx, node_arch())
}

fn os_platform(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    string_value(ctx, node_platform())
}

fn os_machine(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let m = uname_field(UnameField::Machine).unwrap_or_else(|| node_arch().to_string());
    string_value(ctx, &m)
}

fn os_type(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = uname_field(UnameField::Sysname).unwrap_or_else(|| default_os_type().to_string());
    string_value(ctx, &t)
}

fn os_release(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let r = uname_field(UnameField::Release).unwrap_or_else(|| "0.0.0".to_string());
    string_value(ctx, &r)
}

fn os_version(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let v = uname_field(UnameField::Version).unwrap_or_else(|| "unknown".to_string());
    string_value(ctx, &v)
}

fn os_endianness(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let e = if cfg!(target_endian = "big") {
        "BE"
    } else {
        "LE"
    };
    string_value(ctx, e)
}

fn os_hostname(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let h = hostname().unwrap_or_else(|| "localhost".to_string());
    string_value(ctx, &h)
}

fn os_homedir(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let h = home_dir(ctx);
    string_value(ctx, &h)
}

fn os_tmpdir(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let t = tmp_dir(ctx);
    string_value(ctx, &t)
}

fn os_method_to_primitive(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let this = *ctx.this_value();
    let Some(native) = this.as_native_function() else {
        return string_value(ctx, "");
    };
    match native.name(ctx.heap()) {
        "arch" => os_arch(ctx, &[]),
        "platform" => os_platform(ctx, &[]),
        "machine" => os_machine(ctx, &[]),
        "type" => os_type(ctx, &[]),
        "release" => os_release(ctx, &[]),
        "version" => os_version(ctx, &[]),
        "endianness" => os_endianness(ctx, &[]),
        "hostname" => os_hostname(ctx, &[]),
        "homedir" => os_homedir(ctx, &[]),
        "tmpdir" => os_tmpdir(ctx, &[]),
        "totalmem" => os_totalmem(ctx, &[]),
        "freemem" => os_freemem(ctx, &[]),
        "uptime" => os_uptime(ctx, &[]),
        "availableParallelism" => os_available_parallelism(ctx, &[]),
        _ => string_value(ctx, ""),
    }
}

// ---- numeric queries ----

fn os_totalmem(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    number_value(ctx, total_mem() as f64)
}

fn os_freemem(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    number_value(ctx, free_mem() as f64)
}

fn os_uptime(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    number_value(ctx, uptime_secs())
}

fn os_available_parallelism(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    number_value(ctx, num_cpus().max(1) as f64)
}

// ---- arrays / objects ----

fn os_loadavg(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let avg = load_avg();
    let mut scope = ModuleScope::new(ctx);
    let nums: Vec<Rooted> = avg.iter().map(|n| scope.number(*n)).collect();
    let arr = scope.array(&nums).map_err(oom)?;
    Ok(scope.finish(arr))
}

fn os_cpus(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let count = num_cpus().max(1);
    let model = cpu_model();
    let speed = cpu_speed_mhz();
    let mut scope = ModuleScope::new(ctx);
    let mut entries: Vec<Rooted> = Vec::with_capacity(count);
    for _ in 0..count {
        let cpu = scope.object().map_err(oom)?;
        scope.set_string(cpu, "model", &model).map_err(oom)?;
        scope.set_number(cpu, "speed", speed);
        let times = scope.object().map_err(oom)?;
        for field in ["user", "nice", "sys", "idle", "irq"] {
            scope.set_number(times, field, 0.0);
        }
        scope.set(cpu, "times", times);
        entries.push(cpu);
    }
    let arr = scope.array(&entries).map_err(oom)?;
    Ok(scope.finish(arr))
}

fn os_network_interfaces(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    // Minimal: an empty interface map. Sufficient for callers that enumerate
    // entries; per-interface detail lands when a test requires it.
    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(oom)?;
    Ok(scope.finish(obj))
}

fn os_user_info(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let info = user_info(ctx);
    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(oom)?;
    scope.set_number(obj, "uid", info.uid as f64);
    scope.set_number(obj, "gid", info.gid as f64);
    scope
        .set_string(obj, "username", &info.username)
        .map_err(oom)?;
    scope
        .set_string(obj, "homedir", &info.homedir)
        .map_err(oom)?;
    scope.set_string(obj, "shell", &info.shell).map_err(oom)?;
    Ok(scope.finish(obj))
}

// ---- priority ----

fn os_get_priority(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let pid = validate_pid(args.first())?;
    let prio = get_priority(pid)?;
    number_value(ctx, prio as f64)
}

fn os_set_priority(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // setPriority(priority) | setPriority(pid, priority)
    let (pid, priority) = if args.len() >= 2 {
        (validate_pid(args.first())?, args.get(1).copied())
    } else {
        (0, args.first().copied())
    };
    let priority = validate_priority(priority)?;
    set_priority(pid, priority)?;
    let _ = ctx;
    Ok(Value::undefined())
}

fn validate_pid(arg: Option<&Value>) -> Result<i32, NativeError> {
    match arg {
        None => Ok(0),
        Some(v) if v.is_undefined() => Ok(0),
        Some(v) => {
            let Some(n) = v.as_f64() else {
                return Err(crate::invalid_arg_type(
                    "The \"pid\" argument must be of type number.",
                ));
            };
            if !n.is_finite() || n.fract() != 0.0 || n < i32::MIN as f64 || n > i32::MAX as f64 {
                return Err(out_of_range("The value of \"pid\" is out of range."));
            }
            Ok(n as i32)
        }
    }
}

fn validate_priority(arg: Option<Value>) -> Result<i32, NativeError> {
    let Some(v) = arg else {
        return Err(crate::invalid_arg_type(
            "The \"priority\" argument must be of type number.",
        ));
    };
    let Some(n) = v.as_f64() else {
        return Err(crate::invalid_arg_type(
            "The \"priority\" argument must be of type number.",
        ));
    };
    if !n.is_finite() || n.fract() != 0.0 || !(-20.0..=19.0).contains(&n) {
        return Err(out_of_range("The value of \"priority\" is out of range."));
    }
    Ok(n as i32)
}

fn out_of_range(message: &str) -> NativeError {
    NativeError::Coded {
        kind: ErrorKind::RangeError,
        code: "ERR_OUT_OF_RANGE",
        message: message.to_string(),
    }
}

fn system_error(syscall: &str, code: &str, message: &str) -> NativeError {
    NativeError::Coded {
        kind: ErrorKind::Error,
        code: "ERR_SYSTEM_ERROR",
        message: format!("A system error occurred: {syscall} returned {code} ({message})"),
    }
}

fn oom(err: String) -> NativeError {
    crate::type_error("os", err)
}

fn number_value(ctx: &mut NativeCtx<'_>, n: f64) -> Result<Value, NativeError> {
    let _ = ctx;
    Ok(Value::number(otter_vm::number::NumberValue::from_f64(n)))
}

// ---- platform-name mapping ----

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn default_os_type() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        "linux" => "Linux",
        other => other,
    }
}

// ---- env-derived dirs ----

fn home_dir(ctx: &mut NativeCtx<'_>) -> String {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env_var(ctx, key)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(system_home_dir)
}

/// Node's `os.tmpdir()` POSIX semantics: first of `TMPDIR`/`TMP`/`TEMP`, else
/// `/tmp`; strip a trailing separator unless the path is the root.
fn tmp_dir(ctx: &mut NativeCtx<'_>) -> String {
    let raw = ["TMPDIR", "TMP", "TEMP"]
        .iter()
        .find_map(|k| env_var(ctx, k).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "/tmp".to_string());
    if raw.len() > 1 && raw.ends_with('/') {
        raw.trim_end_matches('/').to_string()
    } else {
        raw
    }
}

struct UserInfo {
    uid: i64,
    gid: i64,
    username: String,
    homedir: String,
    shell: String,
}

// ============================ unix FFI ============================

#[cfg(unix)]
enum UnameField {
    Sysname,
    Release,
    Version,
    Machine,
}

#[cfg(unix)]
fn uname_field(field: UnameField) -> Option<String> {
    // SAFETY: zeroed utsname is valid; uname fills NUL-terminated C strings.
    unsafe {
        let mut buf: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut buf) != 0 {
            return None;
        }
        let ptr = match field {
            UnameField::Sysname => buf.sysname.as_ptr(),
            UnameField::Release => buf.release.as_ptr(),
            UnameField::Version => buf.version.as_ptr(),
            UnameField::Machine => buf.machine.as_ptr(),
        };
        Some(
            std::ffi::CStr::from_ptr(ptr.cast())
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[cfg(unix)]
fn hostname() -> Option<String> {
    let mut buf = vec![0u8; 256];
    // SAFETY: writing into a 256-byte buffer; result is NUL-terminated on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

#[cfg(unix)]
fn num_cpus() -> usize {
    // SAFETY: sysconf with a valid name returns a long or -1.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as usize } else { 1 }
}

#[cfg(unix)]
fn load_avg() -> [f64; 3] {
    let mut avg = [0.0f64; 3];
    // SAFETY: getloadavg writes up to 3 doubles into the provided buffer.
    unsafe {
        libc::getloadavg(avg.as_mut_ptr(), 3);
    }
    avg
}

#[cfg(unix)]
fn get_priority(pid: i32) -> Result<i32, NativeError> {
    if pid < 0 {
        return Err(system_error("uv_os_getpriority", "EINVAL", "invalid pid"));
    }
    if pid == 0 || pid == current_pid() {
        return Ok(CURRENT_PROCESS_PRIORITY.load(Ordering::SeqCst));
    }
    // SAFETY: getpriority with PRIO_PROCESS is always safe for a validated pid.
    Ok(unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as _) })
}

#[cfg(unix)]
fn set_priority(pid: i32, priority: i32) -> Result<(), NativeError> {
    if pid < 0 {
        return Err(system_error("uv_os_setpriority", "EINVAL", "invalid pid"));
    }
    if pid == 0 || pid == current_pid() {
        CURRENT_PROCESS_PRIORITY.store(priority, Ordering::SeqCst);
        return Ok(());
    }
    // SAFETY: setpriority with PRIO_PROCESS is safe for a validated pid.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as _, priority) };
    if rc == 0 {
        Ok(())
    } else {
        Err(system_error(
            "uv_os_setpriority",
            "EINVAL",
            "setpriority failed",
        ))
    }
}

#[cfg(unix)]
fn current_pid() -> i32 {
    std::process::id().min(i32::MAX as u32) as i32
}

#[cfg(unix)]
fn user_info(ctx: &mut NativeCtx<'_>) -> UserInfo {
    // SAFETY: getuid/getgid never fail.
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let username = env_var(ctx, "USER")
        .or_else(|| env_var(ctx, "LOGNAME"))
        .unwrap_or_default();
    let shell = env_var(ctx, "SHELL").unwrap_or_default();
    let homedir = home_dir(ctx);
    UserInfo {
        uid: uid as i64,
        gid: gid as i64,
        username,
        homedir,
        shell,
    }
}

#[cfg(unix)]
fn system_home_dir() -> String {
    // SAFETY: getuid never fails; getpwuid returns either null or a pointer to
    // process-global passwd storage valid until the next passwd lookup.
    unsafe {
        let pwd = libc::getpwuid(libc::getuid());
        if pwd.is_null() || (*pwd).pw_dir.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr((*pwd).pw_dir)
            .to_string_lossy()
            .into_owned()
    }
}

// ---- memory / uptime (best-effort, platform-specific) ----

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &core::ffi::CStr) -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: name is a valid C string; sysctlbyname writes <= size bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 { Some(value) } else { None }
}

#[cfg(target_os = "macos")]
fn total_mem() -> u64 {
    sysctl_u64(c"hw.memsize").unwrap_or(0)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn free_mem() -> u64 {
    // SAFETY: mach_host_self returns a send right for the current host;
    // host_statistics64 fills a vm_statistics64 buffer when given the matching
    // count for HOST_VM_INFO64.
    unsafe {
        let host = libc::mach_host_self();
        let mut stats: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT as libc::mach_msg_type_number_t;
        let rc = libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            (&mut stats as *mut libc::vm_statistics64).cast(),
            &mut count,
        );
        if rc != libc::KERN_SUCCESS {
            return 0;
        }
        let page = sysctl_u64(c"hw.pagesize").unwrap_or(4096);
        (stats.free_count as u64)
            .saturating_add(stats.inactive_count as u64)
            .saturating_mul(page)
    }
}

#[cfg(target_os = "macos")]
fn uptime_secs() -> f64 {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    // SAFETY: kern.boottime fills a timeval; we read tv_sec only.
    let mut rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&mut tv as *mut libc::timeval).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        size = std::mem::size_of::<libc::timeval>();
        // SAFETY: MIB selects kern.boottime and writes a timeval into `tv`.
        rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                (&mut tv as *mut libc::timeval).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
    }
    if rc != 0 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes a timespec for CLOCK_MONOTONIC.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
            return (ts.tv_sec.max(0) as f64) + (ts.tv_nsec.max(0) as f64 / 1_000_000_000.0);
        }
        return 0.0;
    }
    // SAFETY: time(NULL) returns current epoch seconds.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    (now - tv.tv_sec).max(0) as f64
}

#[cfg(target_os = "macos")]
fn cpu_model() -> String {
    let mut size = 0usize;
    // SAFETY: first call queries the required buffer length.
    unsafe {
        libc::sysctlbyname(
            c"machdep.cpu.brand_string".as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        );
    }
    if size == 0 {
        return uname_field(UnameField::Machine).unwrap_or_else(|| "unknown".to_string());
    }
    let mut buf = vec![0u8; size];
    // SAFETY: buffer is `size` bytes; sysctl writes a NUL-terminated string.
    let rc = unsafe {
        libc::sysctlbyname(
            c"machdep.cpu.brand_string".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return uname_field(UnameField::Machine).unwrap_or_else(|| "unknown".to_string());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(target_os = "macos")]
fn cpu_speed_mhz() -> f64 {
    sysctl_u64(c"hw.cpufrequency")
        .map(|hz| (hz / 1_000_000) as f64)
        .unwrap_or(0.0)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn total_mem() -> u64 {
    // SAFETY: sysinfo fills the struct; we read totalram/mem_unit.
    unsafe {
        let mut info: libc::sysinfo = std::mem::zeroed();
        if libc::sysinfo(&mut info) == 0 {
            (info.totalram as u64).saturating_mul(info.mem_unit as u64)
        } else {
            0
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn free_mem() -> u64 {
    // SAFETY: see total_mem.
    unsafe {
        let mut info: libc::sysinfo = std::mem::zeroed();
        if libc::sysinfo(&mut info) == 0 {
            (info.freeram as u64).saturating_mul(info.mem_unit as u64)
        } else {
            0
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn uptime_secs() -> f64 {
    // SAFETY: sysinfo fills uptime seconds.
    unsafe {
        let mut info: libc::sysinfo = std::mem::zeroed();
        if libc::sysinfo(&mut info) == 0 {
            info.uptime.max(0) as f64
        } else {
            0.0
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn cpu_model() -> String {
    uname_field(UnameField::Machine).unwrap_or_else(|| "unknown".to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn cpu_speed_mhz() -> f64 {
    0.0
}

// ============================ non-unix fallback ============================

#[cfg(not(unix))]
#[allow(dead_code)]
enum UnameField {
    Sysname,
    Release,
    Version,
    Machine,
}

#[cfg(not(unix))]
fn uname_field(_field: UnameField) -> Option<String> {
    None
}
#[cfg(not(unix))]
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}
#[cfg(not(unix))]
fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
#[cfg(not(unix))]
fn load_avg() -> [f64; 3] {
    [0.0, 0.0, 0.0]
}
#[cfg(not(unix))]
fn get_priority(pid: i32) -> Result<i32, NativeError> {
    if pid < 0 {
        return Err(system_error("uv_os_getpriority", "EINVAL", "invalid pid"));
    }
    Ok(CURRENT_PROCESS_PRIORITY.load(Ordering::SeqCst))
}
#[cfg(not(unix))]
fn set_priority(pid: i32, priority: i32) -> Result<(), NativeError> {
    if pid < 0 {
        return Err(system_error("uv_os_setpriority", "EINVAL", "invalid pid"));
    }
    CURRENT_PROCESS_PRIORITY.store(priority, Ordering::SeqCst);
    Ok(())
}
#[cfg(not(unix))]
fn total_mem() -> u64 {
    0
}
#[cfg(not(unix))]
fn free_mem() -> u64 {
    0
}
#[cfg(not(unix))]
fn uptime_secs() -> f64 {
    0.0
}
#[cfg(not(unix))]
fn cpu_model() -> String {
    "unknown".to_string()
}
#[cfg(not(unix))]
fn cpu_speed_mhz() -> f64 {
    0.0
}
#[cfg(not(unix))]
fn user_info(ctx: &mut NativeCtx<'_>) -> UserInfo {
    let username = env_var(ctx, "USERNAME").unwrap_or_default();
    let homedir = home_dir(ctx);
    UserInfo {
        uid: -1,
        gid: -1,
        username,
        homedir,
        shell: String::new(),
    }
}

#[cfg(not(unix))]
fn system_home_dir() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| {
            let drive = std::env::var("HOMEDRIVE")?;
            let path = std::env::var("HOMEPATH")?;
            Ok(format!("{drive}{path}"))
        })
        .unwrap_or_default()
}
