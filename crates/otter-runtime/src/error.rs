//! Public error model for the new engine.
//!
//! [`OtterError`] is the **only** error type the public API
//! surfaces. It is `#[non_exhaustive]`, derives
//! [`thiserror::Error`] + [`serde::Serialize`] /
//! [`serde::Deserialize`], and serializes to the current JSON wire
//! format.
//!
//! # Contents
//! - [`OtterError`] — top-level error enum.
//! - [`ConfigError`] — companion enum for `OtterError::Config`.
//! - [`IoErrorKind`] — small mapped subset of [`std::io::ErrorKind`].
//! - [`OtterError::to_json`] — convenience for CLI `--json` output.
//!
//! # Invariants
//! - Format changes update every producer, consumer, fixture, and document in
//!   the same patch.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Diagnostic;

/// Public error enum.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OtterError {
    /// `RuntimeBuilder::build` failed.
    #[error("invalid runtime configuration: {reason}")]
    Config {
        /// Specific configuration problem.
        reason: ConfigError,
    },
    /// A hosted builtin module's namespace installer failed.
    #[error("hosted module '{specifier}' failed to install: {message}")]
    HostedModule {
        /// Module specifier, for example `otter:kv`.
        specifier: String,
        /// Installer failure detail.
        message: String,
    },
    /// Filesystem / module loader error.
    #[error("io error reading {}: {message}", .path.display())]
    Io {
        /// Path that triggered the error.
        path: PathBuf,
        /// Mapped subset of [`std::io::ErrorKind`].
        #[serde(rename = "io_kind")]
        kind: IoErrorKind,
        /// Underlying message.
        message: String,
    },
    /// File extension is not one of the foundation extensions.
    #[error("unsupported source kind for {}: extension {extension:?}", .path.display())]
    SourceKind {
        /// The file path.
        path: PathBuf,
        /// The unsupported extension (lowercase, no leading dot).
        extension: String,
    },
    /// Compile-time diagnostics (parse / TS erasure / lower).
    #[error("compile failed with {} diagnostic(s)", .diagnostics.len())]
    Compile {
        /// Non-empty list of diagnostics.
        diagnostics: Vec<Diagnostic>,
    },
    /// A catchable JS error escaped the script.
    #[error("runtime error: {}", .diagnostic.message)]
    Runtime {
        /// Structured diagnostic payload. Boxed to keep [`OtterError`] small
        /// (the diagnostic is the enum's largest variant).
        diagnostic: Box<Diagnostic>,
    },
    /// Configured timeout fired.
    #[error("timeout after {} ms", .elapsed_ms)]
    Timeout {
        /// Wall-clock elapsed at the moment of timeout, in
        /// milliseconds (JSON-friendly).
        elapsed_ms: u64,
    },
    /// Heap cap was hit.
    #[error("out of memory: {requested_bytes} requested, limit {heap_limit_bytes}")]
    OutOfMemory {
        /// Bytes requested by the rejected allocation.
        requested_bytes: u64,
        /// Configured heap limit (`0` = disabled).
        heap_limit_bytes: u64,
    },
    /// A guarded operation was denied.
    #[error("capability denied: {capability}")]
    Capability {
        /// Name of the capability (`fs_read`, `net`, …).
        capability: String,
        /// Optional human-readable detail.
        detail: Option<String>,
    },
    /// Cooperative cancellation observed.
    #[error("interrupted")]
    Interrupted,
    /// Internal bug. CI hard-fail.
    #[error("internal error ({code}): {message}")]
    Internal {
        /// Stable error code (e.g., `VM_BYTECODE_INVARIANT`).
        code: String,
        /// Human-readable detail.
        message: String,
    },
}

impl OtterError {
    /// Convenience: build the public timeout variant from a
    /// [`Duration`].
    #[must_use]
    pub fn timeout_after(elapsed: Duration) -> Self {
        Self::Timeout {
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        }
    }

    /// Map an [`otter_gc::OutOfMemory`] onto the public
    /// [`OtterError::OutOfMemory`] variant. Cage exhaustion and
    /// per-heap cap rejections both surface here.
    #[must_use]
    pub fn from_gc_oom(err: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }

    /// Serialize to stable JSON wire format.
    ///
    /// # Errors
    /// Returns [`serde_json::Error`] if serialization fails (none of
    /// the variants can fail under normal conditions; the result is
    /// `Result` so callers can propagate cleanly).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let envelope = ErrorEnvelope { error: self };
        serde_json::to_string(&envelope)
    }

    /// Pretty-printed variant of [`Self::to_json`].
    ///
    /// # Errors
    /// See [`Self::to_json`].
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        let envelope = ErrorEnvelope { error: self };
        let mut s = serde_json::to_string_pretty(&envelope)?;
        s.push('\n');
        Ok(s)
    }

    /// Recommended CLI exit code.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            OtterError::Compile { .. } | OtterError::Runtime { .. } => 1,
            OtterError::Config { .. }
            | OtterError::HostedModule { .. }
            | OtterError::SourceKind { .. }
            | OtterError::Io { .. } => 2,
            OtterError::Capability { .. } => 3,
            OtterError::Timeout { .. } => 4,
            OtterError::OutOfMemory { .. } => 5,
            OtterError::Interrupted => 130,
            OtterError::Internal { .. } => 64,
        }
    }
}

impl From<otter_gc::OutOfMemory> for OtterError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        Self::from_gc_oom(err)
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    error: &'a OtterError,
}

/// Companion enum for [`OtterError::Config`].
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfigError {
    /// `max_heap_bytes` could not be honored.
    #[error("invalid heap limit: {message}")]
    InvalidHeapLimit {
        /// Detail.
        message: String,
    },
    /// `timeout` could not be honored.
    #[error("invalid timeout: {message}")]
    InvalidTimeout {
        /// Detail.
        message: String,
    },
    /// `max_stack_depth` could not be honored.
    #[error("invalid stack depth limit: {message}")]
    InvalidStackDepth {
        /// Detail.
        message: String,
    },
    /// Capability set is internally inconsistent.
    #[error("conflicting capabilities: {message}")]
    ConflictingCapabilities {
        /// Detail.
        message: String,
    },
}

/// Mapped subset of [`std::io::ErrorKind`] used by
/// [`OtterError::Io`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IoErrorKind {
    /// File not found.
    NotFound,
    /// Permission denied.
    PermissionDenied,
    /// Anything else.
    Other,
}

impl IoErrorKind {
    /// Map from [`std::io::ErrorKind`].
    #[must_use]
    pub fn from_std(kind: std::io::ErrorKind) -> Self {
        use std::io::ErrorKind::*;
        match kind {
            NotFound => Self::NotFound,
            PermissionDenied => Self::PermissionDenied,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_serializes_with_ms() {
        let err = OtterError::Timeout { elapsed_ms: 1234 };
        let json = err.to_json().unwrap();
        assert!(json.contains("\"kind\":\"timeout\""));
        assert!(json.contains("\"elapsed_ms\":1234"));
    }

    #[test]
    fn config_invalid_stack_depth_round_trip() {
        let err = OtterError::Config {
            reason: ConfigError::InvalidStackDepth {
                message: "must be > 0".to_string(),
            },
        };
        let json = err.to_json().unwrap();
        let de: ErrorEnvelopeOwned = serde_json::from_str(&json).unwrap();
        assert!(matches!(de.error, OtterError::Config { .. }));
    }

    #[derive(Debug, Deserialize)]
    struct ErrorEnvelopeOwned {
        error: OtterError,
    }

    #[test]
    fn exit_codes_match_adr() {
        assert_eq!(
            OtterError::Compile {
                diagnostics: vec![Diagnostic::syntax("x")]
            }
            .exit_code(),
            1
        );
        assert_eq!(
            OtterError::Capability {
                capability: "fs_read".to_string(),
                detail: None,
            }
            .exit_code(),
            3
        );
        assert_eq!(OtterError::Timeout { elapsed_ms: 0 }.exit_code(), 4);
        assert_eq!(
            OtterError::OutOfMemory {
                requested_bytes: 0,
                heap_limit_bytes: 0
            }
            .exit_code(),
            5
        );
    }
}
