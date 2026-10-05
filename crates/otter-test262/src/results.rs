//! Owned per-test conformance outcomes shared by workers and reports.
//!
//! # Contents
//! - [`TestResult`] is the sole canonical per-test row.
//! - [`Outcome`] retains complete failure, timeout, crash and heap diagnostics.
//! - [`SkipReason`] identifies the exact source of a runner policy skip.
//!
//! # Invariants
//! Every selected test has one normalized corpus-relative path and one final
//! outcome. A skip is recorded explicitly, never inferred from an absent row.
//! This data owns its strings and remains independent of runtime handles.
//!
//! # See also
//! - [`crate::runner`] creates rows; [`crate::report`] validates their coverage.

use serde::{Deserialize, Deserializer, Serialize};

/// The exact policy that prevents a test from executing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkipReason {
    /// A configured unsupported frontmatter feature.
    Feature {
        /// Configured feature token.
        feature: String,
    },
    /// A configured unsupported frontmatter flag.
    Flag {
        /// Configured flag token.
        flag: String,
    },
    /// The first matching ignored-test configuration pattern.
    Ignored {
        /// First matching configured path substring.
        pattern: String,
    },
    /// The first matching known-panic configuration pattern.
    KnownPanic {
        /// First matching configured path substring.
        pattern: String,
    },
    /// Input exceeds the runner's source-size bound.
    SourceTooLarge {
        /// Actual source size.
        bytes: u64,
        /// Maximum accepted source size.
        limit: u64,
    },
    /// The source has no Test262 frontmatter block.
    MissingFrontmatter,
    /// Conflicting strictness flags leave no executable variant.
    NoStrictnessVariant,
}

impl SkipReason {
    /// Human-readable policy detail; the structured reason remains canonical.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Feature { feature } => format!("unsupported feature: {feature}"),
            Self::Flag { flag } => format!("unsupported flag: {flag}"),
            Self::Ignored { pattern } => format!("ignored by config: {pattern}"),
            Self::KnownPanic { pattern } => format!("known panic: {pattern}"),
            Self::SourceTooLarge { bytes, limit } => {
                format!("source too large: {bytes} bytes exceeds {limit}")
            }
            Self::MissingFrontmatter => "no frontmatter".to_owned(),
            Self::NoStrictnessVariant => "no strictness variant".to_owned(),
        }
    }
}

/// Final outcome of one test, including both required strictness variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// All required variants conform, including expected negative errors.
    Pass,
    /// A conformance failure and its optional complete engine stack.
    Fail {
        /// Complete failure detail.
        reason: String,
        /// Complete engine stack, when available.
        #[serde(deserialize_with = "required_optional")]
        stack: Option<String>,
    },
    /// A policy skip with its exact typed cause.
    Skipped {
        /// Exact policy that prevented execution.
        reason: SkipReason,
    },
    /// Panic or isolated process failure detail.
    Crash {
        /// Panic or isolated process failure detail.
        panic: String,
    },
    /// The configured per-test budget that fired, in milliseconds.
    Timeout {
        /// Configured budget that expired.
        ms: u64,
    },
    /// The requested allocation size reported by the heap cap.
    OutOfMemory {
        /// Requested allocation size.
        bytes: u64,
    },
}

impl Outcome {
    /// Compact display label used by human reports.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail { .. } => "fail",
            Self::Skipped { .. } => "skip",
            Self::Crash { .. } => "crash",
            Self::Timeout { .. } => "timeout",
            Self::OutOfMemory { .. } => "oom",
        }
    }

    /// Preserve the outcome's policy/failure detail in a human view.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Pass => None,
            Self::Fail { reason, .. } => Some(reason.clone()),
            Self::Skipped { reason } => Some(reason.detail()),
            Self::Crash { panic } => Some(panic.clone()),
            Self::Timeout { ms } => Some(format!("timeout after {ms} ms")),
            Self::OutOfMemory { bytes } => Some(format!("oom: {bytes} bytes requested")),
        }
    }

    /// Whether this is an attempted test with a failing outcome.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        !matches!(self, Self::Pass | Self::Skipped { .. })
    }
}

/// Sole owned row written by workers and retained in canonical reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestResult {
    /// Normalized path relative to `vendor/test262/test/`.
    pub path: String,
    /// Frontmatter specification section, when present.
    pub esid: Option<String>,
    /// Frontmatter feature tokens in their original order.
    pub features: Vec<String>,
    /// Final test outcome; no path is implicitly passing or skipped.
    pub outcome: Outcome,
    /// Total elapsed time for this test, in milliseconds.
    pub wall_ms: u64,
}

fn required_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl<'de> Deserialize<'de> for TestResult {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            path: String,
            #[serde(deserialize_with = "required_optional")]
            esid: Option<String>,
            features: Vec<String>,
            outcome: Outcome,
            wall_ms: u64,
        }
        let fields = Fields::deserialize(deserializer)?;
        let row = Self {
            path: fields.path,
            esid: fields.esid,
            features: fields.features,
            outcome: fields.outcome,
            wall_ms: fields.wall_ms,
        };
        row.validate().map_err(serde::de::Error::custom)?;
        Ok(row)
    }
}

impl TestResult {
    /// Validate the current row contract at worker admission and report construction.
    pub fn validate(&self) -> Result<(), String> {
        let path = &self.path;
        if path.is_empty()
            || !path.ends_with(".js")
            || path.ends_with("_FIXTURE.js")
            || path.contains('\\')
            || path.contains('\0')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(format!("noncanonical test path {path:?}"));
        }
        if let Outcome::Skipped {
            reason: SkipReason::SourceTooLarge { bytes, limit },
        } = &self.outcome
        {
            if bytes <= limit {
                return Err("source-too-large reason must exceed its stated limit".to_owned());
            }
        }
        Ok(())
    }
}
