//! Native half of `node:dns`: host name resolution.
//!
//! # Contents
//! - [`dns_cjs_value`] installs the module's JavaScript shim.
//! - `lookupHost` resolves a name to addresses through the host resolver.
//! - `lookupService` maps an address back to a host name.
//!
//! # Invariants
//! - Name resolution is a network operation and is gated by the `net`
//!   capability, checked against the name being resolved.
//! - Resolution runs on the calling thread the way the synchronous filesystem
//!   natives do; the shim is what makes the JavaScript surface asynchronous.
//! - Only the host resolver is used. Query methods that need a DNS client of
//!   our own (`resolveAny`, `resolveMx`, …) are absent rather than faked.
//!
//! # See also
//! - `dns.js`

use std::net::ToSocketAddrs;

use otter_runtime::{
    CapabilitySet, RuntimeLocal, RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope,
    RuntimeTaskSpawner, RuntimeValue, runtime_type_error,
};

/// Build the CommonJS export of `node:dns`.
///
/// # Errors
/// Returns a native error when the shim fails to allocate or evaluate.
pub fn dns_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: RuntimeLocal<'scope>,
    require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let native = build_native(scope, capabilities)?;
    let globals = scope.global_this();
    scope.set(globals, "__otterDnsNative", native)?;
    otter_runtime::run_builtin_cjs_shim(scope, "node:dns", include_str!("dns.js"), module, require)
}

fn build_native<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;

    let lookup_caps = capabilities.clone();
    let lookup = scope.native_closure(
        "lookupHost",
        2,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _captures: &[RuntimeValue]| {
            lookup_host(ctx, args, &lookup_caps)
        },
    )?;
    scope.set(object, "lookupHost", lookup)?;

    let service_caps = capabilities.clone();
    let service = scope.native_closure(
        "lookupService",
        2,
        &[],
        move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _captures: &[RuntimeValue]| {
            lookup_service(ctx, args, &service_caps)
        },
    )?;
    scope.set(object, "lookupService", service)?;

    // The lookup hint flags are the platform's own `AI_*` values, which differ
    // between systems; Node exposes whatever the host defines.
    for (name, value) in address_info_flags() {
        let value = scope.number(f64::from(value));
        scope.set(object, name, value)?;
    }
    Ok(object)
}

/// Resolve `hostname` to a list of `{ address, family }` records, in the order
/// the host resolver returns them.
fn lookup_host(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let hostname = string_arg(ctx, args, 0)?;
    let family = args.get(1).and_then(|value| value.as_f64()).unwrap_or(0.0) as i32;

    if !capabilities.net.matches(&hostname) {
        return Err(runtime_type_error(
            "dns.lookup",
            format!("permission denied for '{hostname}'"),
        ));
    }

    // `ToSocketAddrs` wants a port; the resolver ignores it and the shim never
    // reports it.
    let resolved = (hostname.as_str(), 0u16).to_socket_addrs().map_err(|_| {
        coded_error(
            "ENOTFOUND",
            format!("getaddrinfo ENOTFOUND {hostname}"),
            "getaddrinfo",
            Some(hostname.clone()),
        )
    })?;

    let records: Vec<(String, i32)> = resolved
        .filter(|address| match family {
            4 => address.is_ipv4(),
            6 => address.is_ipv6(),
            _ => true,
        })
        .map(|address| {
            let family = if address.is_ipv4() { 4 } else { 6 };
            (address.ip().to_string(), family)
        })
        .collect();

    if records.is_empty() {
        return Err(coded_error(
            "ENOTFOUND",
            format!("getaddrinfo ENOTFOUND {hostname}"),
            "getaddrinfo",
            Some(hostname),
        ));
    }

    ctx.scope(|mut scope| {
        let array = scope.array(records.len())?;
        for (index, (address, family)) in records.iter().enumerate() {
            let entry = scope.object()?;
            let address = scope.string(address)?;
            scope.set(entry, "address", address)?;
            let family = scope.number(f64::from(*family));
            scope.set(entry, "family", family)?;
            scope.set_index(array, index, entry)?;
        }
        Ok(scope.finish(array))
    })
}

/// Map an address and port back to `{ hostname, service }`. Without a reverse
/// resolver the address stands in for the name, which is what a host with no
/// PTR record answers anyway.
fn lookup_service(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let address = string_arg(ctx, args, 0)?;
    let port = args.get(1).and_then(|value| value.as_f64()).unwrap_or(0.0) as u16;

    if !capabilities.net.matches(&address) {
        return Err(runtime_type_error(
            "dns.lookupService",
            format!("permission denied for '{address}'"),
        ));
    }

    let parsed: std::net::IpAddr = address.parse().map_err(|_| {
        coded_error(
            "EINVAL",
            format!("getnameinfo EINVAL {address}"),
            "getnameinfo",
            Some(address.clone()),
        )
    })?;
    let service = match port {
        80 => "http",
        443 => "https",
        22 => "ssh",
        21 => "ftp",
        25 => "smtp",
        _ => "",
    };

    ctx.scope(|mut scope| {
        let result = scope.object()?;
        let hostname = scope.string(&parsed.to_string())?;
        scope.set(result, "hostname", hostname)?;
        let service = scope.string(service)?;
        scope.set(result, "service", service)?;
        Ok(scope.finish(result))
    })
}

fn string_arg(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    index: usize,
) -> Result<String, RuntimeNativeError> {
    args.get(index)
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|value| value.to_lossy_string(ctx.heap()))
        .ok_or_else(|| runtime_type_error("dns", "expected a string argument".to_string()))
}

fn coded_error(
    code: &'static str,
    message: String,
    syscall: &'static str,
    hostname: Option<String>,
) -> RuntimeNativeError {
    RuntimeNativeError::Syscall {
        code,
        message,
        syscall,
        path: hostname,
        dest: None,
        errno: 0,
    }
}

/// `getaddrinfo` hint flags as this platform defines them.
#[cfg(unix)]
fn address_info_flags() -> [(&'static str, i32); 3] {
    [
        ("ADDRCONFIG", libc::AI_ADDRCONFIG),
        ("V4MAPPED", libc::AI_V4MAPPED),
        ("ALL", libc::AI_ALL),
    ]
}

#[cfg(not(unix))]
fn address_info_flags() -> [(&'static str, i32); 3] {
    [("ADDRCONFIG", 1024), ("V4MAPPED", 2048), ("ALL", 256)]
}
