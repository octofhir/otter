//! Declared install-security settings from `package.json#otter`.
//!
//! This module owns the *syntax* of the project's install policy: what a user
//! may write, how it serializes, and nothing else. Every field is optional so a
//! manifest that omits the section round-trips byte-identically, and so the
//! consumer can tell "not configured" apart from "configured to the default".
//! Resolving these declarations into an effective policy — including the
//! [`InstallSettings::paranoid`] bundle — belongs to the package manager.
//!
//! # Contents
//! - [`OtterManifestSection`] — the `otter` object in `package.json`.
//! - [`InstallSettings`] — declared install-security settings.
//! - [`AdvisoryCheck`] — how a malicious-package advisory lookup failure is
//!   treated.
//! - [`TrustPolicy`] — how a loss of publish provenance is treated.
//!
//! # Invariants
//! - Every setting is `Option`-shaped except [`InstallSettings::allow_builds`],
//!   whose emptiness is itself the "nothing approved" answer.
//! - Approvals are keyed by package name and carry an explicit boolean, so a
//!   reviewed-and-refused package is distinguishable from an unreviewed one.
//! - Serialization skips empty values, so writing an approval back into a
//!   manifest never introduces unrelated keys.
//!
//! # See also
//! - [`crate::PackageManifest`] for the manifest this section hangs off.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The `otter` object in `package.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtterManifestSection {
    /// Install-security settings.
    #[serde(default, skip_serializing_if = "InstallSettings::is_empty")]
    pub install: InstallSettings,
}

impl OtterManifestSection {
    /// `true` when the section carries no declarations at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.install.is_empty()
    }
}

/// Declared install-security settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallSettings {
    /// Force the strict bundle regardless of how each setting below is
    /// declared individually.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paranoid: Option<bool>,
    /// Per-package lifecycle-script approvals, keyed by package name.
    ///
    /// `true` approves the package's install lifecycle scripts, `false`
    /// records a reviewed refusal. An absent key means unreviewed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allow_builds: BTreeMap<String, bool>,
    /// How a malicious-package advisory lookup is treated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory_check: Option<AdvisoryCheck>,
    /// Minimum age, in hours, a published version must have before it may be
    /// selected. `0` disables the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_release_age_hours: Option<u64>,
    /// Fail the install when the age gate leaves no satisfying version,
    /// instead of falling back to the newest version old enough to pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_release_age_strict: Option<bool>,
    /// How a loss of publish provenance is treated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_policy: Option<TrustPolicy>,
    /// Fail when a tarball ships without a registry-published integrity
    /// digest, instead of warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_store_integrity: Option<bool>,
}

impl InstallSettings {
    /// `true` when nothing at all is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// How a malicious-package advisory lookup is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdvisoryCheck {
    /// Do not query the advisory database at all.
    Off,
    /// Query it; a lookup failure warns and the install continues.
    #[default]
    Warn,
    /// Query it; a lookup failure fails the install.
    Required,
}

/// How a loss of publish provenance is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustPolicy {
    /// Provenance is not consulted.
    Off,
    /// A package that carried publish provenance may not lose it.
    #[default]
    NoDowngrade,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_section_round_trips_as_an_empty_object() {
        let section = OtterManifestSection::default();
        assert!(section.is_empty());
        assert_eq!(serde_json::to_string(&section).unwrap(), "{}");
    }

    #[test]
    fn settings_parse_from_camel_case_keys() {
        let settings: InstallSettings = serde_json::from_str(
            r#"{
                "paranoid": true,
                "allowBuilds": { "esbuild": true, "sharp": false },
                "advisoryCheck": "required",
                "minimumReleaseAgeHours": 48,
                "minimumReleaseAgeStrict": true,
                "trustPolicy": "no-downgrade",
                "strictStoreIntegrity": true
            }"#,
        )
        .unwrap();
        assert_eq!(settings.paranoid, Some(true));
        assert_eq!(settings.allow_builds.get("esbuild"), Some(&true));
        assert_eq!(settings.allow_builds.get("sharp"), Some(&false));
        assert_eq!(settings.advisory_check, Some(AdvisoryCheck::Required));
        assert_eq!(settings.minimum_release_age_hours, Some(48));
        assert_eq!(settings.minimum_release_age_strict, Some(true));
        assert_eq!(settings.trust_policy, Some(TrustPolicy::NoDowngrade));
        assert_eq!(settings.strict_store_integrity, Some(true));
    }

    #[test]
    fn declared_settings_serialize_only_what_was_declared() {
        let mut settings = InstallSettings::default();
        settings.allow_builds.insert("esbuild".to_string(), true);
        assert_eq!(
            serde_json::to_string(&settings).unwrap(),
            r#"{"allowBuilds":{"esbuild":true}}"#
        );
    }
}
