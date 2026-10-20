//! The host clock and time zone that `Temporal.Now` and `Date` read.
//!
//! # Contents
//! - [`HostSystem`] — the clock and zone behind every `Temporal.Now` call.
//! - [`now`] — a `Temporal.Now` view of the host.
//! - [`host_time_zone_identifier`] — the zone the host runs in.
//!
//! # Invariants
//! - The zone is `TZ` when it is set (a leading `:` and a path up to
//!   `zoneinfo/` stripped; set but empty is `UTC`); otherwise the zone
//!   `/etc/localtime` links to; otherwise, on Linux, the name in
//!   `/etc/timezone`; otherwise `UTC`.
//!   An identifier the zone database does not know is `UTC`, as in node.
//! - On unix the lookup is one environment read and at most one `readlink`
//!   and one file read: no platform framework is loaded for it.
//! - The clock is the system wall clock.
//!
//! # See also
//! - [`super::now`] — `Temporal.Now`.
//! - `crate::date::LocalTimeZone` — the zone `Date` resolves once per isolate.

use temporal_rs::host::{HostClock, HostHooks, HostTimeZone};
use temporal_rs::provider::TimeZoneProvider;
use temporal_rs::unix_time::EpochNanoseconds;
use temporal_rs::{TemporalError, TemporalResult, TimeZone};

/// The host's wall clock and time zone.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSystem;

impl HostHooks for HostSystem {}

impl HostClock for HostSystem {
    fn get_host_epoch_nanoseconds(&self) -> TemporalResult<EpochNanoseconds> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| TemporalError::general("the system clock is before 1970"))
            .map(|elapsed| EpochNanoseconds::from(elapsed.as_nanos() as i128))
    }
}

impl HostTimeZone for HostSystem {
    fn get_host_time_zone(
        &self,
        provider: &(impl TimeZoneProvider + ?Sized),
    ) -> TemporalResult<TimeZone> {
        let identifier = host_time_zone_identifier();
        Ok(
            TimeZone::try_from_identifier_str_with_provider(&identifier, provider)
                .unwrap_or_else(|_| TimeZone::utc_with_provider(provider)),
        )
    }
}

/// A `Temporal.Now` view of the host.
#[must_use]
pub const fn now() -> temporal_rs::now::Now<HostSystem> {
    temporal_rs::now::Now::new(HostSystem)
}

/// The IANA identifier of the zone the host runs in.
#[must_use]
pub fn host_time_zone_identifier() -> String {
    if let Some(zone) = std::env::var_os("TZ")
        && let Some(zone) = zone.to_str()
    {
        let zone = zone.strip_prefix(':').unwrap_or(zone);
        if zone.is_empty() {
            return "UTC".to_string();
        }
        return zone_from_path(zone).unwrap_or(zone).to_string();
    }
    system_zone().unwrap_or_else(|| "UTC".to_string())
}

/// The identifier a `…/zoneinfo/Area/City` path names.
fn zone_from_path(path: &str) -> Option<&str> {
    path.rsplit_once("zoneinfo/").map(|(_, zone)| zone)
}

#[cfg(unix)]
fn system_zone() -> Option<String> {
    if let Ok(target) = std::fs::read_link("/etc/localtime")
        && let Some(zone) = target.to_str().and_then(zone_from_path)
    {
        return Some(zone.to_string());
    }
    #[cfg(not(target_vendor = "apple"))]
    if let Ok(name) = std::fs::read_to_string("/etc/timezone") {
        let name = name.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(not(unix))]
fn system_zone() -> Option<String> {
    iana_time_zone::get_timezone().ok()
}

#[cfg(test)]
mod tests {
    use super::zone_from_path;

    #[test]
    fn a_zoneinfo_path_names_its_zone() {
        assert_eq!(
            zone_from_path("/var/db/timezone/zoneinfo/Europe/Moscow"),
            Some("Europe/Moscow")
        );
        assert_eq!(
            zone_from_path("/usr/share/zoneinfo/America/Argentina/Buenos_Aires"),
            Some("America/Argentina/Buenos_Aires")
        );
        assert_eq!(zone_from_path("Europe/Paris"), None);
    }
}
