//! Owned, versioned JIT artifact bundles.
//!
//! # Contents
//! - [`JitArtifactMetadata`] and [`JitArtifactManifest`] — stable identity and
//!   file inventory for one successful native compilation.
//! - [`JitArtifactFile`] — one fixed-name owned payload returned by a compiler.
//! - [`JitArtifactBundle`] — validated manifest plus payload files.
//! - [`JitArtifactBatch`] — bounded capture transferred across the runtime
//!   boundary after a top-level execution.
//!
//! # Invariants
//! - Capture is default-off. Disabled state owns no bundle vector and compilers
//!   do not clone executable bytes or format tier input unless explicitly
//!   requested.
//! - File names are a closed enum, so an artifact cannot escape the directory
//!   selected by its host.
//! - Every bundle owns all strings and bytes. It contains no GC handles,
//!   executable pointers, isolate borrows, sinks, locks, or registries.
//! - Exact [`JitArtifactFileName::Code`] bytes are runtime-local. Portable
//!   comparisons use the required normalized and relocation files. The
//!   normalized stream is semantic data and is never executable.
//! - This module performs no I/O. The outer host owns atomic persistence and
//!   write failures.
//!
//! # See also
//! - [`crate::jit`] for the compiler-hook transport.
//! - [`crate::jit_debug`] for the independent structured event stream.

use serde::Serialize;

use crate::jit_debug::{JitDebugRequest, JitDebugTarget, JitDebugTier};

/// Wire-schema version for [`JitArtifactManifest`].
pub const JIT_ARTIFACT_SCHEMA_VERSION: u32 = 1;

/// Maximum successful compile bundles retained by one top-level capture.
pub const JIT_ARTIFACT_BUNDLE_LIMIT: usize = 1_024;

/// Maximum owned payload bytes retained by one top-level capture.
pub const JIT_ARTIFACT_BYTE_LIMIT: usize = 64 * 1024 * 1024;

const ALL_PAYLOAD_FILES: [JitArtifactFileName; 10] = [
    JitArtifactFileName::Bytecode,
    JitArtifactFileName::TemplatePlan,
    JitArtifactFileName::OptimizedIr,
    JitArtifactFileName::Code,
    JitArtifactFileName::NormalizedCode,
    JitArtifactFileName::Assembly,
    JitArtifactFileName::CodeMap,
    JitArtifactFileName::Relocations,
    JitArtifactFileName::Deopt,
    JitArtifactFileName::Safepoints,
];

/// Owned source identity copied only when artifact capture is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitArtifactIdentity {
    /// Source-level or synthesized function name.
    pub function_name: String,
    /// Source module URL.
    pub module: String,
}

/// Fixed payload names allowed inside one compile directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JitArtifactFileName {
    /// Canonical bytecode listing.
    Bytecode,
    /// Template tier's already-built lowering plan.
    TemplatePlan,
    /// Optimizing tier's already-built backend-neutral unit.
    OptimizedIr,
    /// Exact runtime-local executable bytes.
    Code,
    /// Portable code comparison stream with relocations normalized.
    NormalizedCode,
    /// Annotated target assembly.
    Assembly,
    /// Native-offset correlation records.
    CodeMap,
    /// Symbolic baked-address records.
    Relocations,
    /// Optimizing deoptimization metadata.
    Deopt,
    /// Moving-GC safepoint metadata.
    Safepoints,
}

impl JitArtifactFileName {
    /// Stable relative file name used by manifests and hosts.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bytecode => "bytecode.txt",
            Self::TemplatePlan => "template-plan.txt",
            Self::OptimizedIr => "optimized-ir.txt",
            Self::Code => "code.bin",
            Self::NormalizedCode => "code-normalized.bin",
            Self::Assembly => "asm.txt",
            Self::CodeMap => "code-map.json",
            Self::Relocations => "relocations.json",
            Self::Deopt => "deopt.json",
            Self::Safepoints => "safepoints.json",
        }
    }
}

/// Identity and size facts supplied by one successful compiler invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitArtifactMetadata {
    /// Stable target description for the produced code.
    pub target: String,
    /// Rust target architecture spelling.
    pub architecture: String,
    /// Target operating-system spelling.
    pub operating_system: String,
    /// Compiler tier that produced the code.
    pub tier: JitDebugTier,
    /// VM-global bytecode function identity.
    pub function_id: u32,
    /// Source-level or synthesized function name.
    pub function_name: String,
    /// Source module URL.
    pub module: String,
    /// Isolate-assigned code-object identity.
    pub code_object_id: u64,
    /// Entry or loop-OSR compile target.
    pub entry: JitDebugTarget,
    /// Encoded bytecode size.
    pub bytecode_bytes: u64,
    /// Final executable mapping size.
    pub code_bytes: u64,
}

/// Versioned file inventory for one successful native compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JitArtifactManifest {
    #[serde(rename = "otterJitArtifactSchemaVersion")]
    schema_version: u32,
    target: String,
    architecture: String,
    operating_system: String,
    tier: JitDebugTier,
    function_id: u32,
    function_name: String,
    module: String,
    code_object_id: u64,
    entry: JitDebugTarget,
    bytecode_bytes: u64,
    code_bytes: u64,
    exact_code_is_runtime_local: bool,
    files_present: Vec<String>,
    files_absent: Vec<String>,
}

impl JitArtifactManifest {
    /// Artifact manifest wire-schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Stable description of the native target.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Rust target architecture spelling.
    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    /// Rust target operating-system spelling.
    #[must_use]
    pub fn operating_system(&self) -> &str {
        &self.operating_system
    }

    /// Compiler tier that produced the bundle.
    #[must_use]
    pub const fn tier(&self) -> JitDebugTier {
        self.tier
    }

    /// VM-global function identity.
    #[must_use]
    pub const fn function_id(&self) -> u32 {
        self.function_id
    }

    /// Source-level or synthesized function name.
    #[must_use]
    pub fn function_name(&self) -> &str {
        &self.function_name
    }

    /// Source module URL.
    #[must_use]
    pub fn module(&self) -> &str {
        &self.module
    }

    /// Isolate-assigned code-object identity.
    #[must_use]
    pub const fn code_object_id(&self) -> u64 {
        self.code_object_id
    }

    /// Entry or loop-OSR target that initiated this compile.
    #[must_use]
    pub const fn entry(&self) -> JitDebugTarget {
        self.entry
    }

    /// Exact encoded bytecode size.
    #[must_use]
    pub const fn bytecode_bytes(&self) -> u64 {
        self.bytecode_bytes
    }

    /// Exact finalized machine-code size.
    #[must_use]
    pub const fn code_bytes(&self) -> u64 {
        self.code_bytes
    }

    /// Stable names of payloads present in the compile directory.
    #[must_use]
    pub fn files_present(&self) -> &[String] {
        &self.files_present
    }

    /// Stable names of payloads intentionally absent from the directory.
    #[must_use]
    pub fn files_absent(&self) -> &[String] {
        &self.files_absent
    }
}

/// One owned payload inside a compile bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitArtifactFile {
    name: JitArtifactFileName,
    contents: Vec<u8>,
}

impl JitArtifactFile {
    /// Construct a UTF-8 text or JSON payload.
    #[must_use]
    pub fn text(name: JitArtifactFileName, contents: String) -> Self {
        Self {
            name,
            contents: contents.into_bytes(),
        }
    }

    /// Construct a binary payload.
    #[must_use]
    pub fn binary(name: JitArtifactFileName, contents: Vec<u8>) -> Self {
        Self { name, contents }
    }

    /// Fixed relative name of this payload.
    #[must_use]
    pub const fn name(&self) -> JitArtifactFileName {
        self.name
    }

    /// Borrow the exact payload bytes.
    #[must_use]
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }
}

/// Structural failure while joining compiler payloads into one bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JitArtifactBuildError {
    /// Two payloads use the same fixed name.
    DuplicateFile(JitArtifactFileName),
    /// Exact `code.bin` is required for every successful native compile.
    MissingCode,
    /// A versioned bundle is missing another required compiler payload.
    MissingRequiredFile(JitArtifactFileName),
    /// `code.bin` length disagrees with the compiler's finalized mapping.
    CodeSizeMismatch {
        /// Length reported in metadata.
        expected: u64,
        /// Actual payload length.
        actual: u64,
    },
    /// A tier supplied the other tier's input representation.
    WrongTierInput,
}

impl std::fmt::Display for JitArtifactBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid JIT artifact bundle: {self:?}")
    }
}

impl std::error::Error for JitArtifactBuildError {}

/// One validated, owned compile directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitArtifactBundle {
    manifest: JitArtifactManifest,
    files: Box<[JitArtifactFile]>,
    retained_bytes: usize,
}

impl JitArtifactBundle {
    /// Validate and join one successful compile's payloads.
    pub fn new(
        metadata: JitArtifactMetadata,
        mut files: Vec<JitArtifactFile>,
    ) -> Result<Self, JitArtifactBuildError> {
        files.sort_by_key(JitArtifactFile::name);
        for pair in files.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(JitArtifactBuildError::DuplicateFile(pair[0].name));
            }
        }
        let code = files
            .iter()
            .find(|file| file.name == JitArtifactFileName::Code)
            .ok_or(JitArtifactBuildError::MissingCode)?;
        for required in [
            JitArtifactFileName::Bytecode,
            JitArtifactFileName::NormalizedCode,
            JitArtifactFileName::CodeMap,
            JitArtifactFileName::Relocations,
            JitArtifactFileName::Safepoints,
        ] {
            if !files.iter().any(|file| file.name == required) {
                return Err(JitArtifactBuildError::MissingRequiredFile(required));
            }
        }
        let actual = u64::try_from(code.contents.len()).unwrap_or(u64::MAX);
        if actual != metadata.code_bytes {
            return Err(JitArtifactBuildError::CodeSizeMismatch {
                expected: metadata.code_bytes,
                actual,
            });
        }
        let has_template = files
            .iter()
            .any(|file| file.name == JitArtifactFileName::TemplatePlan);
        let has_optimized = files
            .iter()
            .any(|file| file.name == JitArtifactFileName::OptimizedIr);
        if match metadata.tier {
            JitDebugTier::Template => !has_template || has_optimized,
            JitDebugTier::Optimizing => has_template || !has_optimized,
        } {
            return Err(JitArtifactBuildError::WrongTierInput);
        }

        let mut files_present = Vec::with_capacity(files.len() + 1);
        files_present.push("manifest.json".to_string());
        files_present.extend(files.iter().map(|file| file.name.as_str().to_string()));
        let files_absent = ALL_PAYLOAD_FILES
            .iter()
            .copied()
            .filter(|name| {
                files
                    .binary_search_by_key(name, JitArtifactFile::name)
                    .is_err()
            })
            .map(|name| name.as_str().to_string())
            .collect();
        let retained_bytes = files.iter().fold(0usize, |total, file| {
            total.saturating_add(file.contents.len())
        });
        let manifest = JitArtifactManifest {
            schema_version: JIT_ARTIFACT_SCHEMA_VERSION,
            target: metadata.target,
            architecture: metadata.architecture,
            operating_system: metadata.operating_system,
            tier: metadata.tier,
            function_id: metadata.function_id,
            function_name: metadata.function_name,
            module: metadata.module,
            code_object_id: metadata.code_object_id,
            entry: metadata.entry,
            bytecode_bytes: metadata.bytecode_bytes,
            code_bytes: metadata.code_bytes,
            exact_code_is_runtime_local: true,
            files_present,
            files_absent,
        };
        Ok(Self {
            manifest,
            files: files.into_boxed_slice(),
            retained_bytes,
        })
    }

    /// Borrow the versioned manifest.
    #[must_use]
    pub const fn manifest(&self) -> &JitArtifactManifest {
        &self.manifest
    }

    /// Borrow payloads in stable file-name order.
    #[must_use]
    pub fn files(&self) -> &[JitArtifactFile] {
        &self.files
    }

    /// Borrow one fixed-name payload.
    #[must_use]
    pub fn file(&self, name: JitArtifactFileName) -> Option<&JitArtifactFile> {
        self.files
            .binary_search_by_key(&name, JitArtifactFile::name)
            .ok()
            .map(|index| &self.files[index])
    }

    /// Owned payload bytes counted against the batch bound.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

/// Bounded artifact capture from one or more sequential top-level executions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitArtifactBatch {
    bundles: Vec<JitArtifactBundle>,
    retained_bytes: usize,
    dropped_bundles: u64,
    dropped_bytes: u64,
}

impl JitArtifactBatch {
    fn from_captured(
        bundles: Vec<JitArtifactBundle>,
        retained_bytes: usize,
        dropped_bundles: u64,
        dropped_bytes: u64,
    ) -> Self {
        Self {
            bundles,
            retained_bytes,
            dropped_bundles,
            dropped_bytes,
        }
    }

    /// Borrow compile bundles in capture order.
    #[must_use]
    pub fn bundles(&self) -> &[JitArtifactBundle] {
        &self.bundles
    }

    /// Number of retained payload bytes.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Number of bundles omitted by count or byte bounds.
    #[must_use]
    pub const fn dropped_bundles(&self) -> u64 {
        self.dropped_bundles
    }

    /// Payload bytes belonging to omitted bundles.
    #[must_use]
    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// Whether one or more successful compile bundles were omitted.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.dropped_bundles != 0
    }

    /// Whether no compile bundle was retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bundles.is_empty()
    }

    /// Merge a later capture while preserving order and hard bounds.
    #[must_use]
    pub fn merged(mut self, other: Self) -> Self {
        self.dropped_bundles = self.dropped_bundles.saturating_add(other.dropped_bundles);
        self.dropped_bytes = self.dropped_bytes.saturating_add(other.dropped_bytes);
        for bundle in other.bundles {
            let bytes = bundle.retained_bytes();
            if self.bundles.len() >= JIT_ARTIFACT_BUNDLE_LIMIT
                || self.retained_bytes.saturating_add(bytes) > JIT_ARTIFACT_BYTE_LIMIT
            {
                self.dropped_bundles = self.dropped_bundles.saturating_add(1);
                self.dropped_bytes = self
                    .dropped_bytes
                    .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
                continue;
            }
            self.retained_bytes += bytes;
            self.bundles.push(bundle);
        }
        self
    }
}

/// Isolate-local storage for default-off artifact capture.
#[derive(Debug)]
pub(crate) struct JitArtifactState {
    bundles: Option<Vec<JitArtifactBundle>>,
    retained_bytes: usize,
    dropped_bundles: u64,
    dropped_bytes: u64,
}

impl Default for JitArtifactState {
    fn default() -> Self {
        Self::new(JitDebugRequest::default())
    }
}

impl JitArtifactState {
    pub(crate) fn new(request: JitDebugRequest) -> Self {
        Self {
            bundles: request.artifacts_enabled().then(Vec::new),
            retained_bytes: 0,
            dropped_bundles: 0,
            dropped_bytes: 0,
        }
    }

    pub(crate) fn set_request(&mut self, request: JitDebugRequest) {
        self.bundles = request.artifacts_enabled().then(Vec::new);
        self.retained_bytes = 0;
        self.dropped_bundles = 0;
        self.dropped_bytes = 0;
    }

    pub(crate) fn begin_batch(&mut self) {
        if let Some(bundles) = self.bundles.as_mut() {
            bundles.clear();
        }
        self.retained_bytes = 0;
        self.dropped_bundles = 0;
        self.dropped_bytes = 0;
    }

    pub(crate) fn record(&mut self, bundle: JitArtifactBundle) {
        let Some(bundles) = self.bundles.as_mut() else {
            return;
        };
        let bytes = bundle.retained_bytes();
        if bundles.len() >= JIT_ARTIFACT_BUNDLE_LIMIT
            || self.retained_bytes.saturating_add(bytes) > JIT_ARTIFACT_BYTE_LIMIT
        {
            self.dropped_bundles = self.dropped_bundles.saturating_add(1);
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
            return;
        }
        self.retained_bytes += bytes;
        bundles.push(bundle);
    }

    pub(crate) fn take_batch(&mut self) -> Option<JitArtifactBatch> {
        self.bundles.as_mut().map(|bundles| {
            JitArtifactBatch::from_captured(
                std::mem::take(bundles),
                std::mem::take(&mut self.retained_bytes),
                std::mem::take(&mut self.dropped_bundles),
                std::mem::take(&mut self.dropped_bytes),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn metadata(tier: JitDebugTier, code_bytes: u64) -> JitArtifactMetadata {
        JitArtifactMetadata {
            target: "aarch64-macos".to_string(),
            architecture: "aarch64".to_string(),
            operating_system: "macos".to_string(),
            tier,
            function_id: 7,
            function_name: "hot".to_string(),
            module: "main.js".to_string(),
            code_object_id: 11,
            entry: JitDebugTarget::Osr { pc: 4 },
            bytecode_bytes: 19,
            code_bytes,
        }
    }

    fn bundle() -> JitArtifactBundle {
        JitArtifactBundle::new(
            metadata(JitDebugTier::Template, 4),
            vec![
                JitArtifactFile::text(JitArtifactFileName::TemplatePlan, "plan\n".to_string()),
                JitArtifactFile::text(JitArtifactFileName::Bytecode, "code\n".to_string()),
                JitArtifactFile::binary(JitArtifactFileName::Code, vec![1, 2, 3, 4]),
                JitArtifactFile::binary(JitArtifactFileName::NormalizedCode, b"OTJNCODE".to_vec()),
                JitArtifactFile::text(JitArtifactFileName::CodeMap, "{}\n".to_string()),
                JitArtifactFile::text(JitArtifactFileName::Relocations, "{}\n".to_string()),
                JitArtifactFile::text(JitArtifactFileName::Safepoints, "{}\n".to_string()),
            ],
        )
        .expect("valid bundle")
    }

    #[test]
    fn manifest_is_versioned_and_inventory_is_explicit() {
        let bundle = bundle();
        let value = serde_json::to_value(bundle.manifest()).expect("serialize manifest");
        assert_eq!(
            value,
            json!({
                "otterJitArtifactSchemaVersion": 1,
                "target": "aarch64-macos",
                "architecture": "aarch64",
                "operatingSystem": "macos",
                "tier": "template",
                "functionId": 7,
                "functionName": "hot",
                "module": "main.js",
                "codeObjectId": 11,
                "entry": {"kind": "osr", "pc": 4},
                "bytecodeBytes": 19,
                "codeBytes": 4,
                "exactCodeIsRuntimeLocal": true,
                "filesPresent": [
                    "manifest.json",
                    "bytecode.txt",
                    "template-plan.txt",
                    "code.bin",
                    "code-normalized.bin",
                    "code-map.json",
                    "relocations.json",
                    "safepoints.json"
                ],
                "filesAbsent": [
                    "optimized-ir.txt",
                    "asm.txt",
                    "deopt.json"
                ]
            })
        );
    }

    #[test]
    fn disabled_state_owns_no_bundle_buffer() {
        let state = JitArtifactState::default();
        assert!(state.bundles.is_none());
    }

    #[test]
    fn enabled_state_drains_owned_batches() {
        let mut state = JitArtifactState::new(JitDebugRequest::artifacts());
        state.record(bundle());
        let batch = state.take_batch().expect("capture enabled");
        assert_eq!(batch.bundles().len(), 1);
        assert!(!batch.truncated());
        assert!(
            state
                .take_batch()
                .expect("capture remains enabled")
                .is_empty()
        );
    }

    #[test]
    fn merging_an_empty_truncated_batch_preserves_drop_metadata() {
        let retained =
            JitArtifactBatch::from_captured(vec![bundle()], bundle().retained_bytes(), 0, 0);
        let dropped = JitArtifactBatch::from_captured(Vec::new(), 0, 2, 17);
        let merged = retained.merged(dropped);

        assert_eq!(merged.bundles().len(), 1);
        assert_eq!(merged.dropped_bundles(), 2);
        assert_eq!(merged.dropped_bytes(), 17);
        assert!(merged.truncated());
    }

    #[test]
    fn bundle_rejects_duplicate_or_wrong_tier_payloads() {
        assert_eq!(
            JitArtifactBundle::new(
                metadata(JitDebugTier::Template, 1),
                vec![
                    JitArtifactFile::binary(JitArtifactFileName::Code, vec![0]),
                    JitArtifactFile::binary(JitArtifactFileName::Code, vec![0]),
                    JitArtifactFile::text(JitArtifactFileName::TemplatePlan, String::new()),
                    JitArtifactFile::text(JitArtifactFileName::Bytecode, String::new()),
                    JitArtifactFile::binary(JitArtifactFileName::NormalizedCode, Vec::new()),
                    JitArtifactFile::text(JitArtifactFileName::CodeMap, String::new()),
                    JitArtifactFile::text(JitArtifactFileName::Relocations, String::new()),
                    JitArtifactFile::text(JitArtifactFileName::Safepoints, String::new()),
                ],
            )
            .expect_err("duplicate code"),
            JitArtifactBuildError::DuplicateFile(JitArtifactFileName::Code)
        );
        assert_eq!(
            JitArtifactBundle::new(
                metadata(JitDebugTier::Optimizing, 1),
                vec![
                    JitArtifactFile::binary(JitArtifactFileName::Code, vec![0]),
                    JitArtifactFile::text(JitArtifactFileName::TemplatePlan, String::new()),
                    JitArtifactFile::text(JitArtifactFileName::Bytecode, String::new()),
                    JitArtifactFile::binary(JitArtifactFileName::NormalizedCode, Vec::new()),
                    JitArtifactFile::text(JitArtifactFileName::CodeMap, String::new()),
                    JitArtifactFile::text(JitArtifactFileName::Relocations, String::new()),
                    JitArtifactFile::text(JitArtifactFileName::Safepoints, String::new()),
                ],
            )
            .expect_err("wrong tier input"),
            JitArtifactBuildError::WrongTierInput
        );
    }
}
