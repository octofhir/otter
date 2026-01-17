//! DNS extension module.
//!
//! Provides node:dns compatible DNS resolution.

use otter_runtime::Extension;
use otter_runtime::extension::op_async;
use serde_json::json;

use crate::dns;

/// Create the DNS extension.
pub fn extension() -> Extension {
    Extension::new("dns")
        .with_ops(vec![
            op_async("__otter_dns_lookup", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                let family = args.get(1).and_then(|v| v.as_u64()).map(|f| f as u8);

                match dns::lookup(hostname, family).await {
                    Ok(result) => Ok(json!({
                        "address": result.address,
                        "family": result.family
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                let rrtype = args.get(1).and_then(|v| v.as_str()).unwrap_or("A");

                dns::resolve(hostname, rrtype)
                    .await
                    .map_err(|e| otter_runtime::error::JscError::internal(e.to_string()))
            }),
            op_async("__otter_dns_resolve4", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve4(hostname).await {
                    Ok(addrs) => Ok(json!(addrs)),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve6", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve6(hostname).await {
                    Ok(addrs) => Ok(json!(addrs)),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve_mx", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve_mx(hostname).await {
                    Ok(records) => Ok(json!(
                        records
                            .iter()
                            .map(|r| json!({
                                "exchange": r.exchange,
                                "priority": r.priority
                            }))
                            .collect::<Vec<_>>()
                    )),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve_txt", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve_txt(hostname).await {
                    Ok(records) => Ok(json!(records)),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve_ns", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve_ns(hostname).await {
                    Ok(records) => Ok(json!(records)),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve_cname", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve_cname(hostname).await {
                    Ok(records) => Ok(json!(records)),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve_srv", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve_srv(hostname).await {
                    Ok(records) => Ok(json!(
                        records
                            .iter()
                            .map(|r| json!({
                                "name": r.name,
                                "port": r.port,
                                "priority": r.priority,
                                "weight": r.weight
                            }))
                            .collect::<Vec<_>>()
                    )),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_resolve_soa", |_ctx, args| async move {
                let hostname = args.first().and_then(|v| v.as_str()).ok_or_else(|| {
                    otter_runtime::error::JscError::internal("hostname is required")
                })?;

                match dns::resolve_soa(hostname).await {
                    Ok(record) => Ok(json!({
                        "nsname": record.nsname,
                        "hostmaster": record.hostmaster,
                        "serial": record.serial,
                        "refresh": record.refresh,
                        "retry": record.retry,
                        "expire": record.expire,
                        "minttl": record.minttl
                    })),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
            op_async("__otter_dns_reverse", |_ctx, args| async move {
                let ip = args
                    .first()
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| otter_runtime::error::JscError::internal("ip is required"))?;

                match dns::reverse(ip).await {
                    Ok(hostnames) => Ok(json!(hostnames)),
                    Err(e) => Err(otter_runtime::error::JscError::internal(e.to_string())),
                }
            }),
        ])
        .with_js(include_str!("dns.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "dns");
        assert!(ext.js_code().is_some());
    }
}
