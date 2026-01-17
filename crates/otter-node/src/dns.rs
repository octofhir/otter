//! DNS resolution module.
//!
//! Provides Node.js-compatible DNS resolution APIs.

use hickory_resolver::TokioResolver;
use std::net::IpAddr;
use std::sync::OnceLock;
use thiserror::Error;

/// Global resolver instance.
static RESOLVER: OnceLock<TokioResolver> = OnceLock::new();

/// Get or create the global resolver.
fn get_resolver() -> Result<&'static TokioResolver, DnsError> {
    match RESOLVER.get() {
        Some(r) => Ok(r),
        None => {
            let resolver = TokioResolver::builder_tokio()
                .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?
                .build();
            Ok(RESOLVER.get_or_init(|| resolver))
        }
    }
}

/// DNS resolution errors.
#[derive(Debug, Error)]
pub enum DnsError {
    #[error("DNS resolution failed: {0}")]
    ResolutionFailed(String),

    #[error("No addresses found for hostname")]
    NoAddresses,

    #[error("Invalid hostname: {0}")]
    InvalidHostname(String),

    #[error("Invalid IP address: {0}")]
    InvalidIpAddress(String),

    #[error("Record type not found")]
    RecordNotFound,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of a DNS lookup operation.
#[derive(Debug, Clone)]
pub struct LookupResult {
    /// The resolved IP address.
    pub address: String,
    /// Address family (4 for IPv4, 6 for IPv6).
    pub family: u8,
}

/// MX record result.
#[derive(Debug, Clone)]
pub struct MxRecord {
    /// Mail server hostname.
    pub exchange: String,
    /// Priority (lower is higher priority).
    pub priority: u16,
}

/// SRV record result.
#[derive(Debug, Clone)]
pub struct SrvRecord {
    /// Target hostname.
    pub name: String,
    /// Port number.
    pub port: u16,
    /// Priority (lower is higher priority).
    pub priority: u16,
    /// Weight for load balancing.
    pub weight: u16,
}

/// SOA record result.
#[derive(Debug, Clone)]
pub struct SoaRecord {
    /// Primary nameserver.
    pub nsname: String,
    /// Administrator email.
    pub hostmaster: String,
    /// Serial number.
    pub serial: u32,
    /// Refresh interval.
    pub refresh: u32,
    /// Retry interval.
    pub retry: u32,
    /// Expiry time.
    pub expire: u32,
    /// Minimum TTL.
    pub minttl: u32,
}

/// Lookup a hostname and return the first IP address.
///
/// This is equivalent to `dns.lookup()` in Node.js.
pub async fn lookup(hostname: &str, family: Option<u8>) -> Result<LookupResult, DnsError> {
    let resolver = get_resolver()?;

    // Try to resolve based on family preference
    match family {
        Some(4) => {
            // IPv4 only
            let response = resolver
                .ipv4_lookup(hostname)
                .await
                .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

            let addr = response.iter().next().ok_or(DnsError::NoAddresses)?;

            Ok(LookupResult {
                address: addr.0.to_string(),
                family: 4,
            })
        }
        Some(6) => {
            // IPv6 only
            let response = resolver
                .ipv6_lookup(hostname)
                .await
                .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

            let addr = response.iter().next().ok_or(DnsError::NoAddresses)?;

            Ok(LookupResult {
                address: addr.0.to_string(),
                family: 6,
            })
        }
        _ => {
            // Try IPv4 first, then IPv6
            if let Ok(response) = resolver.ipv4_lookup(hostname).await {
                if let Some(addr) = response.iter().next() {
                    return Ok(LookupResult {
                        address: addr.0.to_string(),
                        family: 4,
                    });
                }
            }

            let response = resolver
                .ipv6_lookup(hostname)
                .await
                .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

            let addr = response.iter().next().ok_or(DnsError::NoAddresses)?;

            Ok(LookupResult {
                address: addr.0.to_string(),
                family: 6,
            })
        }
    }
}

/// Resolve all IPv4 addresses for a hostname.
pub async fn resolve4(hostname: &str) -> Result<Vec<String>, DnsError> {
    let resolver = get_resolver()?;
    let response = resolver
        .ipv4_lookup(hostname)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let addrs: Vec<String> = response.iter().map(|a| a.0.to_string()).collect();
    if addrs.is_empty() {
        return Err(DnsError::NoAddresses);
    }
    Ok(addrs)
}

/// Resolve all IPv6 addresses for a hostname.
pub async fn resolve6(hostname: &str) -> Result<Vec<String>, DnsError> {
    let resolver = get_resolver()?;
    let response = resolver
        .ipv6_lookup(hostname)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let addrs: Vec<String> = response.iter().map(|a| a.0.to_string()).collect();
    if addrs.is_empty() {
        return Err(DnsError::NoAddresses);
    }
    Ok(addrs)
}

/// Resolve MX records for a hostname.
pub async fn resolve_mx(hostname: &str) -> Result<Vec<MxRecord>, DnsError> {
    let resolver = get_resolver()?;
    let response = resolver
        .mx_lookup(hostname)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let records: Vec<MxRecord> = response
        .iter()
        .map(|mx| MxRecord {
            exchange: mx.exchange().to_string().trim_end_matches('.').to_string(),
            priority: mx.preference(),
        })
        .collect();

    if records.is_empty() {
        return Err(DnsError::RecordNotFound);
    }
    Ok(records)
}

/// Resolve TXT records for a hostname.
pub async fn resolve_txt(hostname: &str) -> Result<Vec<Vec<String>>, DnsError> {
    let resolver = get_resolver()?;
    let response = resolver
        .txt_lookup(hostname)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let records: Vec<Vec<String>> = response
        .iter()
        .map(|txt| {
            txt.iter()
                .map(|s| String::from_utf8_lossy(s).to_string())
                .collect()
        })
        .collect();

    if records.is_empty() {
        return Err(DnsError::RecordNotFound);
    }
    Ok(records)
}

/// Resolve NS records for a hostname.
pub async fn resolve_ns(hostname: &str) -> Result<Vec<String>, DnsError> {
    let resolver = get_resolver()?;
    let response = resolver
        .ns_lookup(hostname)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let records: Vec<String> = response
        .iter()
        .map(|ns| ns.0.to_string().trim_end_matches('.').to_string())
        .collect();

    if records.is_empty() {
        return Err(DnsError::RecordNotFound);
    }
    Ok(records)
}

/// Resolve CNAME record for a hostname.
pub async fn resolve_cname(hostname: &str) -> Result<Vec<String>, DnsError> {
    let resolver = get_resolver()?;

    // Use lookup to find CNAME
    let response = resolver
        .lookup(hostname, hickory_resolver::proto::rr::RecordType::CNAME)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let records: Vec<String> = response
        .iter()
        .filter_map(|r| {
            r.as_cname()
                .map(|cname| cname.to_string().trim_end_matches('.').to_string())
        })
        .collect();

    if records.is_empty() {
        return Err(DnsError::RecordNotFound);
    }
    Ok(records)
}

/// Resolve SRV records for a hostname.
pub async fn resolve_srv(hostname: &str) -> Result<Vec<SrvRecord>, DnsError> {
    let resolver = get_resolver()?;
    let response = resolver
        .srv_lookup(hostname)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let records: Vec<SrvRecord> = response
        .iter()
        .map(|srv| SrvRecord {
            name: srv.target().to_string().trim_end_matches('.').to_string(),
            port: srv.port(),
            priority: srv.priority(),
            weight: srv.weight(),
        })
        .collect();

    if records.is_empty() {
        return Err(DnsError::RecordNotFound);
    }
    Ok(records)
}

/// Resolve SOA record for a hostname.
pub async fn resolve_soa(hostname: &str) -> Result<SoaRecord, DnsError> {
    let resolver = get_resolver()?;
    let response = resolver
        .soa_lookup(hostname)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let soa = response.iter().next().ok_or(DnsError::RecordNotFound)?;

    Ok(SoaRecord {
        nsname: soa.mname().to_string().trim_end_matches('.').to_string(),
        hostmaster: soa.rname().to_string().trim_end_matches('.').to_string(),
        serial: soa.serial(),
        refresh: soa.refresh() as u32,
        retry: soa.retry() as u32,
        expire: soa.expire() as u32,
        minttl: soa.minimum(),
    })
}

/// Resolve PTR records for an IP address (reverse lookup).
pub async fn reverse(ip: &str) -> Result<Vec<String>, DnsError> {
    let resolver = get_resolver()?;

    let ip_addr: IpAddr = ip
        .parse()
        .map_err(|_| DnsError::InvalidIpAddress(ip.to_string()))?;

    let response = resolver
        .reverse_lookup(ip_addr)
        .await
        .map_err(|e| DnsError::ResolutionFailed(e.to_string()))?;

    let hostnames: Vec<String> = response
        .iter()
        .map(|name| name.to_string().trim_end_matches('.').to_string())
        .collect();

    if hostnames.is_empty() {
        return Err(DnsError::RecordNotFound);
    }
    Ok(hostnames)
}

/// Resolve records of a specific type.
pub async fn resolve(hostname: &str, rrtype: &str) -> Result<serde_json::Value, DnsError> {
    use serde_json::json;

    match rrtype.to_uppercase().as_str() {
        "A" => {
            let addrs = resolve4(hostname).await?;
            Ok(json!(addrs))
        }
        "AAAA" => {
            let addrs = resolve6(hostname).await?;
            Ok(json!(addrs))
        }
        "MX" => {
            let records = resolve_mx(hostname).await?;
            Ok(json!(
                records
                    .iter()
                    .map(|r| json!({
                        "exchange": r.exchange,
                        "priority": r.priority
                    }))
                    .collect::<Vec<_>>()
            ))
        }
        "TXT" => {
            let records = resolve_txt(hostname).await?;
            Ok(json!(records))
        }
        "NS" => {
            let records = resolve_ns(hostname).await?;
            Ok(json!(records))
        }
        "CNAME" => {
            let records = resolve_cname(hostname).await?;
            Ok(json!(records))
        }
        "SRV" => {
            let records = resolve_srv(hostname).await?;
            Ok(json!(
                records
                    .iter()
                    .map(|r| json!({
                        "name": r.name,
                        "port": r.port,
                        "priority": r.priority,
                        "weight": r.weight
                    }))
                    .collect::<Vec<_>>()
            ))
        }
        "SOA" => {
            let record = resolve_soa(hostname).await?;
            Ok(json!({
                "nsname": record.nsname,
                "hostmaster": record.hostmaster,
                "serial": record.serial,
                "refresh": record.refresh,
                "retry": record.retry,
                "expire": record.expire,
                "minttl": record.minttl
            }))
        }
        "PTR" => {
            let records = reverse(hostname).await?;
            Ok(json!(records))
        }
        _ => Err(DnsError::ResolutionFailed(format!(
            "Unknown record type: {}",
            rrtype
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_lookup_google() {
        // This test requires network access
        let result = lookup("google.com", None).await;
        // Don't assert success since it depends on network
        if let Ok(r) = result {
            assert!(r.family == 4 || r.family == 6);
            assert!(!r.address.is_empty());
        }
    }

    #[tokio::test]
    async fn test_lookup_ipv4_only() {
        let result = lookup("google.com", Some(4)).await;
        if let Ok(r) = result {
            assert_eq!(r.family, 4);
        }
    }

    #[tokio::test]
    async fn test_resolve4() {
        let result = resolve4("google.com").await;
        if let Ok(addrs) = result {
            assert!(!addrs.is_empty());
        }
    }

    #[tokio::test]
    async fn test_resolve_mx() {
        let result = resolve_mx("google.com").await;
        if let Ok(records) = result {
            assert!(!records.is_empty());
            for record in &records {
                assert!(!record.exchange.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn test_resolve_generic() {
        let result = resolve("google.com", "A").await;
        if let Ok(value) = result {
            assert!(value.is_array());
        }
    }

    #[test]
    fn test_dns_error_display() {
        let err = DnsError::NoAddresses;
        assert_eq!(err.to_string(), "No addresses found for hostname");

        let err = DnsError::InvalidHostname("test".to_string());
        assert_eq!(err.to_string(), "Invalid hostname: test");
    }
}
