//! Minimum-release-age gate for version selection.
//!
//! A compromised release is most dangerous in the hours right after it is
//! published: that is the window before anyone has looked at it and before an
//! advisory exists. Holding new versions back for a cooling period turns that
//! window into someone else's problem without pinning the project to old code
//! — the gate selects the newest version that is *old enough*, not the oldest
//! one that works.
//!
//! # Contents
//! - [`published_at`] — publish timestamp of one version.
//! - [`is_old_enough`] — whether a version has cleared the cooling window.
//!
//! # Invariants
//! - A version with no publish timestamp is treated as old enough. The gate
//!   protects against fresh releases, and a registry that does not report
//!   times must not become a registry nothing installs from.
//! - Time comparisons are one-directional: a timestamp in the future is not
//!   old enough, and a clock skew that makes everything look old fails open.
//!
//! # See also
//! - [`crate::policy::InstallPolicy::minimum_release_age`] for the window.

use std::time::{Duration, SystemTime};

use crate::policy::InstallPolicy;
use crate::registry::NpmRegistryMetadata;

/// The cooling window as it applies to one install.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReleaseAgeGate {
    window: Duration,
    strict: bool,
    now: SystemTime,
}

impl ReleaseAgeGate {
    /// Build the gate a policy asks for, anchored at `now`.
    pub(crate) fn new(policy: &InstallPolicy, now: SystemTime) -> Self {
        Self {
            window: policy.minimum_release_age,
            strict: policy.minimum_release_age_strict,
            now,
        }
    }

    /// `true` when the gate admits everything.
    pub(crate) fn is_disabled(&self) -> bool {
        self.window.is_zero()
    }

    /// `true` when a rejection must fail the install instead of falling back.
    pub(crate) fn is_strict(&self) -> bool {
        self.strict
    }

    /// Configured window in whole hours, for diagnostics.
    pub(crate) fn window_hours(&self) -> u64 {
        self.window.as_secs() / 3600
    }

    /// `true` when `version` has cleared the window, or the registry reported
    /// no publish time for it.
    pub(crate) fn admits(&self, metadata: &NpmRegistryMetadata, version: &str) -> bool {
        published_at(metadata, version)
            .is_none_or(|published| is_old_enough(published, self.now, self.window))
    }

    /// Age of `version` in whole hours, or `0` when unknown.
    pub(crate) fn version_age_hours(&self, metadata: &NpmRegistryMetadata, version: &str) -> u64 {
        published_at(metadata, version).map_or(0, |published| age_hours(published, self.now))
    }
}

/// Publish timestamp of `version`, when the registry reported one.
#[must_use]
pub(crate) fn published_at(metadata: &NpmRegistryMetadata, version: &str) -> Option<SystemTime> {
    let raw = metadata.time.get(version)?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|stamp| SystemTime::from(stamp.with_timezone(&chrono::Utc)))
}

/// `true` when `published` has cleared the cooling window as of `now`.
#[must_use]
pub(crate) fn is_old_enough(published: SystemTime, now: SystemTime, window: Duration) -> bool {
    if window.is_zero() {
        return true;
    }
    now.duration_since(published).is_ok_and(|age| age >= window)
}

/// Age of `published` as of `now`, in whole hours, saturating at zero for a
/// timestamp in the future.
#[must_use]
pub(crate) fn age_hours(published: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(published)
        .map_or(0, |age| age.as_secs() / 3600)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata_with_time(version: &str, stamp: &str) -> NpmRegistryMetadata {
        serde_json::from_str(&format!(
            r#"{{"name":"tool","versions":{{}},"time":{{"{version}":"{stamp}"}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn missing_or_unparseable_timestamps_do_not_block() {
        let metadata = metadata_with_time("1.0.0", "not a timestamp");
        assert_eq!(published_at(&metadata, "1.0.0"), None);
        assert_eq!(published_at(&metadata, "2.0.0"), None);
    }

    #[test]
    fn a_version_clears_the_window_once_it_is_old_enough() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let window = Duration::from_secs(24 * 3600);
        assert!(is_old_enough(
            now - Duration::from_secs(25 * 3600),
            now,
            window
        ));
        assert!(!is_old_enough(
            now - Duration::from_secs(23 * 3600),
            now,
            window
        ));
    }

    #[test]
    fn a_future_timestamp_is_never_old_enough() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let published = now + Duration::from_secs(3600);
        assert!(!is_old_enough(published, now, Duration::from_secs(3600)));
        assert_eq!(age_hours(published, now), 0);
    }

    #[test]
    fn a_zero_window_admits_everything() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(is_old_enough(now, now, Duration::ZERO));
    }

    #[test]
    fn rfc3339_timestamps_parse_into_system_time() {
        let metadata = metadata_with_time("1.0.0", "2020-01-01T00:00:00.000Z");
        let published = published_at(&metadata, "1.0.0").unwrap();
        assert_eq!(
            published
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_577_836_800
        );
    }
}
