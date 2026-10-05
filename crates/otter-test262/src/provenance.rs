//! Owned identity of the executable and effective conformance policy.
//!
//! # Contents
//! - [`RunnerProvenance`] hashes the actual executable sent to workers.
//! - [`RunConfig`] retains effective limits, tier, isolation and skip policy.
//!
//! # Invariants
//! Shards merge only when this entire identity matches. Diff permits a changed
//! executable, but requires the same target, assertions and effective policy.
//! Selection, concurrency and timestamps are not semantic policy fields.
//!
//! # See also
//! - [`crate::report`] owns this identity beside the canonical test rows.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// Effective per-test policy, independent of batch selection and scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// Effective timeout, in milliseconds.
    pub timeout_ms: u64,
    /// Effective managed heap cap, in bytes; zero disables it.
    pub max_heap_bytes: u64,
    /// Selected execution policy: production-tiered, template or interpreter.
    pub jit_tier: String,
    /// Whether tests restore a per-worker runtime snapshot.
    pub snapshot_isolates: bool,
    /// Ordered unsupported feature policy.
    pub skip_features: Vec<String>,
    /// Ordered unsupported flag policy.
    pub skip_flags: Vec<String>,
    /// Ordered ignored path patterns.
    pub ignored_tests: Vec<String>,
    /// Ordered known-panic path patterns.
    pub known_panics: Vec<String>,
    /// SHA-256 of present whitelisted GC control values; arbitrary environment
    /// values and diagnostic paths are never persisted.
    pub engine_environment: BTreeMap<String, String>,
}

/// Actual worker executable identity; a dirty tree's commit alone is insufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerProvenance {
    /// Lowercase SHA-256 of the executable bytes.
    pub executable_sha256: String,
    /// Actual Cargo compilation target triple.
    pub target: String,
    /// Effective Rust debug-assertion setting for this runner build.
    pub debug_assertions: bool,
    /// Effective semantic policy frozen before launching workers.
    pub semantic_config: RunConfig,
}

impl RunnerProvenance {
    /// Validate the digest, target and bounded current semantic policy fields.
    pub fn validate(&self) -> Result<(), String> {
        let digest = |text: &str| {
            text.len() == 64
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if !digest(&self.executable_sha256) || self.target.is_empty() {
            return Err("invalid runner executable digest/target".to_owned());
        }
        let config = &self.semantic_config;
        if config.timeout_ms > 30_000
            || !matches!(
                config.jit_tier.as_str(),
                "production-tiered" | "template" | "interpreter"
            )
        {
            return Err("invalid effective timeout/tier".to_owned());
        }
        if !config.engine_environment.iter().all(|(key, value)| {
            matches!(key.as_str(), "OTTER_GC_STRESS" | "OTTER_GC_VERIFY") && digest(value)
        }) {
            return Err(
                "engine environment must contain only whitelisted GC value digests".to_owned(),
            );
        }
        Ok(())
    }

    /// Hash the actual executable, retaining the explicit effective policy.
    pub fn capture(executable: &Path, semantic_config: RunConfig) -> std::io::Result<Self> {
        let mut file = std::fs::File::open(executable)?;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(Self {
            executable_sha256: format!("{:x}", digest.finalize()),
            target: env!("OTTER_TEST262_BUILD_TARGET").to_owned(),
            debug_assertions: cfg!(debug_assertions),
            semantic_config,
        })
    }

    /// Whether a before/after comparison uses the same execution conditions.
    #[must_use]
    pub fn comparable_policy(&self, other: &Self) -> bool {
        self.target == other.target
            && self.debug_assertions == other.debug_assertions
            && self.semantic_config == other.semantic_config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_actual_executable_bytes_and_distinguishes_same_source_commit_builds() {
        let first = tempfile::NamedTempFile::new().unwrap();
        let second = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(first.path(), b"abc").unwrap();
        std::fs::write(second.path(), b"changed actual executable").unwrap();
        let a = RunnerProvenance::capture(first.path(), synthetic().semantic_config).unwrap();
        let b = RunnerProvenance::capture(second.path(), a.semantic_config.clone()).unwrap();
        assert_eq!(
            a.executable_sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(a, b);
        assert!(a.comparable_policy(&b));
        assert_eq!(a.target, env!("OTTER_TEST262_BUILD_TARGET"));
        assert_eq!(a.debug_assertions, cfg!(debug_assertions));
    }

    #[test]
    fn arbitrary_environment_keys_and_raw_values_cannot_enter_current_report() {
        let mut identity = synthetic();
        identity
            .semantic_config
            .engine_environment
            .insert("OTTER_SECRET".to_owned(), "0".repeat(64));
        assert!(identity.validate().is_err());
        identity.semantic_config.engine_environment.clear();
        identity.semantic_config.engine_environment.insert(
            "OTTER_GC_STRESS".to_owned(),
            "raw arbitrary text".to_owned(),
        );
        assert!(identity.validate().is_err());
        identity
            .semantic_config
            .engine_environment
            .insert("OTTER_GC_STRESS".to_owned(), "0".repeat(64));
        assert!(identity.validate().is_ok());
    }
}

/// Capture only active GC execution controls without exposing their raw values.
#[must_use]
pub fn engine_environment() -> BTreeMap<String, String> {
    ["OTTER_GC_STRESS", "OTTER_GC_VERIFY"]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key).map(|value| {
                (
                    key.to_owned(),
                    format!("{:x}", Sha256::digest(value.as_encoded_bytes())),
                )
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn synthetic() -> RunnerProvenance {
    RunnerProvenance {
        executable_sha256: "0".repeat(64),
        target: "synthetic-test-target".to_owned(),
        debug_assertions: true,
        semantic_config: RunConfig {
            timeout_ms: 5000,
            max_heap_bytes: 1024,
            jit_tier: "interpreter".to_owned(),
            snapshot_isolates: false,
            skip_features: vec![],
            skip_flags: vec![],
            ignored_tests: vec![],
            known_panics: vec![],
            engine_environment: BTreeMap::new(),
        },
    }
}
