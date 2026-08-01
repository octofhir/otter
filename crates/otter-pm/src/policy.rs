//! Effective install-security policy.
//!
//! The manifest states what a user *declared*; this module answers what the
//! installer must actually *do*. Resolution happens once, at the top of an
//! install, so every gate downstream reads a settled value instead of
//! re-deriving defaults — and so `paranoid` can override an individual
//! declaration in exactly one place.
//!
//! # Contents
//! - [`InstallPolicy`] — the settled policy every gate consults.
//! - [`BuildDecision`] — approval state of one package's lifecycle scripts.
//!
//! # Invariants
//! - Defaults are the secure ones: no dependency lifecycle script runs without
//!   an explicit approval, freshly published versions are held back, and a
//!   package that loses publish provenance fails resolution.
//! - [`InstallSettings::paranoid`] forces the strict bundle on; it never
//!   relaxes a setting a project declared more strictly on its own.
//! - An unreviewed package and a reviewed-and-refused package are distinct
//!   states: the first is worth prompting about, the second is not.
//!
//! # See also
//! - [`otter_pm_manifest::InstallSettings`] for the declared form.
//! - [`crate::lifecycle`] for the gate that consumes [`BuildDecision`].

use std::collections::BTreeMap;
use std::time::Duration;

use otter_pm_manifest::{AdvisoryCheck, InstallSettings, PackageManifest, TrustPolicy};

/// Hours a published version is held back before it may be selected.
const DEFAULT_MINIMUM_RELEASE_AGE_HOURS: u64 = 24;

/// Settled install-security policy for one install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPolicy {
    /// Per-package lifecycle-script approvals, keyed by package name.
    pub allow_builds: BTreeMap<String, bool>,
    /// How a malicious-package advisory lookup is treated.
    pub advisory_check: AdvisoryCheck,
    /// How long a published version is held back before it may be selected.
    pub minimum_release_age: Duration,
    /// Fail rather than fall back when the age gate rejects the newest
    /// satisfying version.
    pub minimum_release_age_strict: bool,
    /// How a loss of publish provenance is treated.
    pub trust_policy: TrustPolicy,
    /// Fail rather than warn when a tarball has no registry-published
    /// integrity digest.
    pub strict_store_integrity: bool,
}

impl Default for InstallPolicy {
    fn default() -> Self {
        Self {
            allow_builds: BTreeMap::new(),
            advisory_check: AdvisoryCheck::Warn,
            minimum_release_age: Duration::from_secs(DEFAULT_MINIMUM_RELEASE_AGE_HOURS * 3600),
            minimum_release_age_strict: false,
            trust_policy: TrustPolicy::NoDowngrade,
            strict_store_integrity: false,
        }
    }
}

impl InstallPolicy {
    /// Resolve declared settings into an effective policy.
    #[must_use]
    pub fn from_settings(settings: &InstallSettings) -> Self {
        let defaults = Self::default();
        let paranoid = settings.paranoid.unwrap_or(false);
        let mut policy = Self {
            allow_builds: settings.allow_builds.clone(),
            advisory_check: settings.advisory_check.unwrap_or(defaults.advisory_check),
            minimum_release_age: settings
                .minimum_release_age_hours
                .map_or(defaults.minimum_release_age, |hours| {
                    Duration::from_secs(hours.saturating_mul(3600))
                }),
            minimum_release_age_strict: settings
                .minimum_release_age_strict
                .unwrap_or(defaults.minimum_release_age_strict),
            trust_policy: settings.trust_policy.unwrap_or(defaults.trust_policy),
            strict_store_integrity: settings
                .strict_store_integrity
                .unwrap_or(defaults.strict_store_integrity),
        };
        if paranoid {
            policy.advisory_check = AdvisoryCheck::Required;
            policy.minimum_release_age_strict = true;
            policy.trust_policy = TrustPolicy::NoDowngrade;
            policy.strict_store_integrity = true;
            if policy.minimum_release_age.is_zero() {
                policy.minimum_release_age = defaults.minimum_release_age;
            }
        }
        policy
    }

    /// Resolve the policy declared by a project manifest.
    #[must_use]
    pub fn from_manifest(manifest: &PackageManifest) -> Self {
        Self::from_settings(&manifest.install_settings())
    }

    /// Approval state of one package's install lifecycle scripts.
    #[must_use]
    pub fn build_decision(&self, package_name: &str) -> BuildDecision {
        match self.allow_builds.get(package_name) {
            Some(true) => BuildDecision::Approved,
            Some(false) => BuildDecision::Refused,
            None => BuildDecision::Unreviewed,
        }
    }

    /// `true` when the age gate is switched off entirely.
    #[must_use]
    pub fn release_age_gate_disabled(&self) -> bool {
        self.minimum_release_age.is_zero()
    }
}

/// Approval state of one package's install lifecycle scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildDecision {
    /// Scripts are approved and will run.
    Approved,
    /// Scripts were reviewed and refused.
    Refused,
    /// Scripts have not been reviewed; they are skipped and reported.
    Unreviewed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_deny_unreviewed_builds_and_hold_fresh_releases() {
        let policy = InstallPolicy::default();
        assert_eq!(policy.build_decision("esbuild"), BuildDecision::Unreviewed);
        assert_eq!(policy.minimum_release_age, Duration::from_secs(24 * 3600));
        assert_eq!(policy.advisory_check, AdvisoryCheck::Warn);
        assert_eq!(policy.trust_policy, TrustPolicy::NoDowngrade);
    }

    #[test]
    fn declared_settings_win_over_defaults() {
        let settings: InstallSettings = serde_json::from_str(
            r#"{
                "allowBuilds": { "esbuild": true, "sharp": false },
                "advisoryCheck": "off",
                "minimumReleaseAgeHours": 0
            }"#,
        )
        .unwrap();
        let policy = InstallPolicy::from_settings(&settings);
        assert_eq!(policy.build_decision("esbuild"), BuildDecision::Approved);
        assert_eq!(policy.build_decision("sharp"), BuildDecision::Refused);
        assert_eq!(policy.build_decision("other"), BuildDecision::Unreviewed);
        assert_eq!(policy.advisory_check, AdvisoryCheck::Off);
        assert!(policy.release_age_gate_disabled());
    }

    #[test]
    fn paranoid_forces_the_strict_bundle_on() {
        let settings: InstallSettings = serde_json::from_str(
            r#"{
                "paranoid": true,
                "advisoryCheck": "off",
                "trustPolicy": "off",
                "minimumReleaseAgeHours": 0,
                "strictStoreIntegrity": false
            }"#,
        )
        .unwrap();
        let policy = InstallPolicy::from_settings(&settings);
        assert_eq!(policy.advisory_check, AdvisoryCheck::Required);
        assert_eq!(policy.trust_policy, TrustPolicy::NoDowngrade);
        assert!(policy.minimum_release_age_strict);
        assert!(policy.strict_store_integrity);
        assert!(!policy.release_age_gate_disabled());
    }

    #[test]
    fn paranoid_keeps_a_stricter_declared_age_window() {
        let settings: InstallSettings =
            serde_json::from_str(r#"{ "paranoid": true, "minimumReleaseAgeHours": 72 }"#).unwrap();
        let policy = InstallPolicy::from_settings(&settings);
        assert_eq!(policy.minimum_release_age, Duration::from_secs(72 * 3600));
    }
}
