//! Owned, default-off diagnostics for JIT compilation and side exits.
//!
//! # Contents
//! - [`JitDebugRequest`] — the immutable capture request copied into a compile
//!   request.
//! - [`JitDebugEvent`] and its helper enums — structured, serializable events
//!   for compilation, inlining, bails, and inline-frame materialization.
//! - [`JitDebugReport`] — one owned, schema-versioned batch of events.
//! - [`JitDebugState`] — isolate-local event storage used by the interpreter.
//!
//! # Invariants
//! - Capture is disabled by default and a disabled [`JitDebugState`] owns no
//!   event vector.
//! - Event payload construction is lazy: [`JitDebugState::record`] accepts a
//!   closure and never calls it while capture is disabled.
//! - Each batch retains at most [`JIT_DEBUG_EVENT_LIMIT`] events. Once full it
//!   counts drops without invoking further payload builders.
//! - Every public DTO owns its strings and collections. No event contains a raw
//!   VM handle, isolate borrow, executable pointer, sink, lock, or registry.
//! - Reports and events are output-only serialized DTOs. Their public
//!   constructors enforce the event cap and derived truncation metadata.
//! - This module performs no I/O. Hosts decide how and where to serialize a
//!   completed report.
//!
//! # See also
//! - [`crate::jit`] for the compiler hook and owned compile request boundary.
//! - [`crate::Interpreter`] for the isolate that owns the corresponding state.

use serde::{Deserialize, Serialize};

/// Wire-schema version for [`JitDebugReport`].
pub const JIT_DEBUG_SCHEMA_VERSION: u32 = 1;

/// Maximum number of events retained by one isolate capture batch.
pub const JIT_DEBUG_EVENT_LIMIT: usize = 16_384;

/// Immutable JIT diagnostics requested by an embedder.
///
/// The request is cheap to copy into every compiler invocation. The default is
/// fully disabled, so ordinary execution does not retain diagnostic events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JitDebugRequest {
    #[serde(default)]
    capture_events: bool,
}

impl JitDebugRequest {
    /// Construct an explicitly disabled request.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            capture_events: false,
        }
    }

    /// Construct a request that captures structured JIT events.
    #[must_use]
    pub const fn events() -> Self {
        Self {
            capture_events: true,
        }
    }

    /// Return whether structured event capture is enabled.
    #[must_use]
    pub const fn events_enabled(self) -> bool {
        self.capture_events
    }
}

/// Native compilation tier associated with a debug event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JitDebugTier {
    /// Template baseline compilation or execution.
    Template,
    /// Feedback-driven optimizing compilation or execution.
    Optimizing,
}

/// Entry point associated with a compile attempt or side exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum JitDebugTarget {
    /// Ordinary function entry.
    Entry,
    /// Synchronous host-to-VM function entry.
    SyncEntry,
    /// On-stack replacement at one loop header.
    Osr {
        /// Logical bytecode PC of the loop header.
        pc: u32,
    },
}

/// Structured outcome of one compiler-hook invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum JitDebugCompileOutcome {
    /// Native code was produced and accepted by the compiler hook.
    Compiled {
        /// Isolate-assigned identity stamped into the code object.
        code_object_id: u64,
        /// Final native code length in bytes.
        code_bytes: u64,
    },
    /// The selected backend or executable memory is unavailable.
    Unavailable,
    /// The function is outside the selected tier's supported subset.
    Unsupported {
        /// Owned compiler explanation.
        reason: String,
    },
    /// The compiler hook returned an internal error.
    Error {
        /// Owned error message.
        message: String,
    },
}

/// Why an inline candidate was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum JitInlineRejectionReason {
    /// The call site observed multiple possible callees.
    Polymorphic,
    /// Feedback named a callee that is absent from the execution context.
    MissingCallee,
    /// The callee cannot use the narrow synchronous inline path.
    Ineligible {
        /// The callee is a generator.
        generator: bool,
        /// The callee is an async function.
        async_function: bool,
        /// The callee is an async generator.
        async_generator: bool,
        /// The callee requires an `arguments` object.
        needs_arguments: bool,
        /// The callee declares a rest parameter.
        has_rest: bool,
        /// The callee contains direct `eval`.
        contains_direct_eval: bool,
        /// The callee is a derived constructor.
        derived_constructor: bool,
        /// The callee creates a nested function.
        makes_function: bool,
    },
    /// No immutable compile snapshot exists for the candidate.
    MissingSnapshot,
}

/// One structured JIT diagnostics event.
///
/// The enum uses an internally tagged representation so report consumers can
/// dispatch on the stable `type` field without parsing human-readable text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum JitDebugEvent {
    /// The VM finished baking feedback into an owned compile snapshot.
    CompilePrepared {
        /// Global bytecode function id.
        function_id: u32,
        /// Source-level or synthesized function name.
        function_name: String,
        /// Native tier about to consume the snapshot.
        tier: JitDebugTier,
        /// Function entry or loop-OSR target.
        target: JitDebugTarget,
        /// Number of frame registers in the function.
        register_count: u32,
        /// Number of declared parameters.
        parameter_count: u32,
        /// Call sites carrying target feedback.
        call_feedback_sites: u32,
        /// Method sites carrying target feedback.
        method_feedback_sites: u32,
        /// Direct-call callee bodies baked into the snapshot.
        inline_callees: u32,
        /// Monomorphic method bodies baked into the snapshot.
        inline_methods: u32,
    },
    /// One direct-call inline candidate was baked or rejected by the VM.
    InlineCandidate {
        /// Function containing the call site.
        caller_function_id: u32,
        /// Logical PC of the call instruction.
        instruction_pc: u32,
        /// Tier for which the candidate was inspected.
        tier: JitDebugTier,
        /// Candidate callee id, when feedback identified one.
        callee_function_id: Option<u32>,
        /// Bake-stage rejection reason. `None` means a candidate snapshot was
        /// made available to the backend, which may still reject it for final
        /// size, arity, or tier-specific eligibility constraints.
        bake_rejection: Option<JitInlineRejectionReason>,
    },
    /// One compiler-hook invocation completed.
    CompileFinished {
        /// Global bytecode function id.
        function_id: u32,
        /// Tier asked to compile the function.
        tier: JitDebugTier,
        /// Function entry or loop-OSR target.
        target: JitDebugTarget,
        /// Typed compiler result.
        outcome: JitDebugCompileOutcome,
    },
    /// Native execution returned to the interpreter at a precise PC.
    Bail {
        /// Global bytecode function id.
        function_id: u32,
        /// Source-level or synthesized function name.
        function_name: String,
        /// Tier that produced the side exit.
        tier: JitDebugTier,
        /// Entry shape used for the native invocation.
        target: JitDebugTarget,
        /// Logical PC at which interpreter execution resumes.
        resume_pc: u32,
        /// Human-readable opcode name, when the PC resolves.
        op_debug: Option<String>,
        /// Human-readable operand rendering, when the PC resolves.
        operands_debug: Option<String>,
    },
    /// One interpreter frame was materialized from an inline deopt record.
    InlineDeoptFrame {
        /// Zero-based position in the reconstructed inline-frame chain.
        index: u32,
        /// Total number of reconstructed inline frames.
        total: u32,
        /// Global bytecode function id of the materialized callee.
        function_id: u32,
        /// Logical PC at which the materialized frame resumes.
        resume_pc: u32,
    },
}

/// Owned, schema-versioned batch of structured JIT diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JitDebugReport {
    #[serde(rename = "otterJitDebugSchemaVersion")]
    schema_version: u32,
    events: Vec<JitDebugEvent>,
    #[serde(rename = "droppedEvents")]
    dropped_events: u64,
    truncated: bool,
}

impl JitDebugReport {
    /// Construct a version-1 report from owned events.
    #[must_use]
    pub fn from_events(events: Vec<JitDebugEvent>) -> Self {
        let dropped_events =
            u64::try_from(events.len().saturating_sub(JIT_DEBUG_EVENT_LIMIT)).unwrap_or(u64::MAX);
        let events = events
            .into_iter()
            .take(JIT_DEBUG_EVENT_LIMIT)
            .collect::<Vec<_>>()
            .into_boxed_slice()
            .into_vec();
        Self::from_captured(events, dropped_events)
    }

    fn from_captured(events: Vec<JitDebugEvent>, dropped_events: u64) -> Self {
        Self {
            schema_version: JIT_DEBUG_SCHEMA_VERSION,
            events,
            dropped_events,
            truncated: dropped_events != 0,
        }
    }

    /// Report wire-schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Borrow the captured events in emission order.
    #[must_use]
    pub fn events(&self) -> &[JitDebugEvent] {
        &self.events
    }

    /// Consume the report and return its owned events.
    #[must_use]
    pub fn into_events(self) -> Vec<JitDebugEvent> {
        self.events
    }

    /// Number of events omitted after the bounded capture filled.
    #[must_use]
    pub const fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    /// Return whether this report omitted one or more events.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Return whether the report contains no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Merge a later report while preserving event order and the hard cap.
    #[must_use]
    pub fn merged(mut self, other: Self) -> Self {
        let available = JIT_DEBUG_EVENT_LIMIT.saturating_sub(self.events.len());
        let appended = available.min(other.events.len());
        let newly_dropped =
            u64::try_from(other.events.len().saturating_sub(appended)).unwrap_or(u64::MAX);
        self.events.extend(other.events.into_iter().take(appended));
        self.dropped_events = self
            .dropped_events
            .saturating_add(other.dropped_events)
            .saturating_add(newly_dropped);
        self.truncated = self.dropped_events != 0;
        self
    }
}

/// Isolate-local storage for default-off JIT diagnostics.
///
/// This state deliberately owns data instead of a callback sink. Compiler hooks
/// remain immutable and sendable, while the single-threaded interpreter records
/// and later transfers complete event batches without synchronization.
#[derive(Debug)]
pub(crate) struct JitDebugState {
    request: JitDebugRequest,
    events: Option<Vec<JitDebugEvent>>,
    dropped_events: u64,
}

impl Default for JitDebugState {
    fn default() -> Self {
        Self::new(JitDebugRequest::default())
    }
}

impl JitDebugState {
    /// Construct state for one immutable capture request.
    pub(crate) fn new(request: JitDebugRequest) -> Self {
        Self {
            request,
            events: request.events_enabled().then(Vec::new),
            dropped_events: 0,
        }
    }

    /// Return the request copied into compiler invocations.
    pub(crate) const fn request(&self) -> JitDebugRequest {
        self.request
    }

    /// Replace the capture request and reset retained events.
    ///
    /// Enabling starts a fresh batch; disabling immediately drops the previous
    /// batch and returns the state to its allocation-free representation.
    pub(crate) fn set_request(&mut self, request: JitDebugRequest) {
        self.request = request;
        self.events = request.events_enabled().then(Vec::new);
        self.dropped_events = 0;
    }

    /// Start a fresh top-level capture while preserving enabled-buffer capacity.
    pub(crate) fn begin_batch(&mut self) {
        if let Some(events) = self.events.as_mut() {
            events.clear();
        }
        self.dropped_events = 0;
    }

    /// Reserve one bounded event slot, counting a drop when already full.
    pub(crate) fn reserve_event(&mut self) -> bool {
        match self.events.as_ref() {
            Some(events) if events.len() < JIT_DEBUG_EVENT_LIMIT => true,
            Some(_) => {
                self.dropped_events = self.dropped_events.saturating_add(1);
                false
            }
            None => false,
        }
    }

    /// Fill a slot returned by [`Self::reserve_event`].
    pub(crate) fn push_reserved(&mut self, event: JitDebugEvent) {
        let events = self
            .events
            .as_mut()
            .expect("reserved JIT debug slot requires enabled capture");
        debug_assert!(events.len() < JIT_DEBUG_EVENT_LIMIT);
        events.push(event);
    }

    /// Lazily record one event when capture is enabled and has capacity.
    ///
    /// `build` is not invoked while disabled or full, so formatting function
    /// names, operands, or compiler explanations has no hidden tail cost.
    pub(crate) fn record(&mut self, build: impl FnOnce() -> JitDebugEvent) {
        if self.reserve_event() {
            self.push_reserved(build());
        }
    }

    /// Drain the current batch into an owned report.
    ///
    /// Disabled state returns `None`. Enabled state returns `Some`, including
    /// when no events were recorded, so callers can distinguish an empty capture
    /// from diagnostics that were never requested.
    pub(crate) fn take_report(&mut self) -> Option<JitDebugReport> {
        self.events.as_mut().map(|events| {
            let dropped_events = std::mem::take(&mut self.dropped_events);
            JitDebugReport::from_captured(std::mem::take(events), dropped_events)
        })
    }
}

impl crate::Interpreter {
    /// Install an explicit default-off JIT diagnostics request.
    pub fn set_jit_debug_request(&mut self, request: JitDebugRequest) {
        self.jit_debug.set_request(request);
    }

    /// Return the diagnostics request copied into compiler-hook invocations.
    #[must_use]
    pub fn jit_debug_request(&self) -> JitDebugRequest {
        self.jit_debug.request()
    }

    /// Start a fresh top-level JIT diagnostics batch.
    pub fn begin_jit_debug_capture(&mut self) {
        self.jit_debug.begin_batch();
    }

    /// Record an owned event without constructing its payload while disabled.
    pub(crate) fn record_jit_debug_event(&mut self, build: impl FnOnce() -> JitDebugEvent) {
        self.jit_debug.record(build);
    }

    /// Reserve a diagnostics slot before computing an expensive payload.
    pub(crate) fn reserve_jit_debug_event(&mut self) -> bool {
        self.jit_debug.reserve_event()
    }

    /// Publish an event after [`Self::reserve_jit_debug_event`] succeeds.
    pub(crate) fn push_reserved_jit_debug_event(&mut self, event: JitDebugEvent) {
        self.jit_debug.push_reserved(event);
    }

    /// Drain the current diagnostics batch into an owned report.
    #[must_use]
    pub fn take_jit_debug_report(&mut self) -> Option<JitDebugReport> {
        self.jit_debug.take_report()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use super::*;

    fn sample_event() -> JitDebugEvent {
        JitDebugEvent::CompileFinished {
            function_id: 7,
            tier: JitDebugTier::Template,
            target: JitDebugTarget::Entry,
            outcome: JitDebugCompileOutcome::Unsupported {
                reason: "unsupported opcode".to_string(),
            },
        }
    }

    #[test]
    fn request_is_disabled_by_default() {
        assert_eq!(JitDebugRequest::default(), JitDebugRequest::disabled());
        assert!(!JitDebugRequest::default().events_enabled());
        assert!(JitDebugRequest::events().events_enabled());
    }

    #[test]
    fn disabled_state_does_not_build_event_payload() {
        let mut state = JitDebugState::default();
        let called = Cell::new(false);

        state.record(|| {
            called.set(true);
            sample_event()
        });

        assert!(!called.get());
        assert_eq!(state.request(), JitDebugRequest::disabled());
        assert!(state.take_report().is_none());
    }

    #[test]
    fn enabled_state_drains_owned_versioned_reports() {
        let mut state = JitDebugState::new(JitDebugRequest::events());
        state.record(sample_event);

        let report = state.take_report().expect("capture is enabled");
        assert_eq!(report.schema_version(), JIT_DEBUG_SCHEMA_VERSION);
        assert_eq!(report.events(), &[sample_event()]);

        let next = state.take_report().expect("capture remains enabled");
        assert!(next.is_empty());
    }

    #[test]
    fn report_serialization_has_stable_schema_and_event_tags() {
        let report = JitDebugReport::from_events(vec![JitDebugEvent::CompilePrepared {
            function_id: 11,
            function_name: "hotLoop".to_string(),
            tier: JitDebugTier::Optimizing,
            target: JitDebugTarget::Osr { pc: 4 },
            register_count: 8,
            parameter_count: 1,
            call_feedback_sites: 2,
            method_feedback_sites: 3,
            inline_callees: 1,
            inline_methods: 0,
        }]);

        assert_eq!(
            serde_json::to_value(report).expect("serialize report"),
            json!({
                "otterJitDebugSchemaVersion": 1,
                "events": [{
                    "type": "compilePrepared",
                    "functionId": 11,
                    "functionName": "hotLoop",
                    "tier": "optimizing",
                    "target": {
                        "kind": "osr",
                        "pc": 4
                    },
                    "registerCount": 8,
                    "parameterCount": 1,
                    "callFeedbackSites": 2,
                    "methodFeedbackSites": 3,
                    "inlineCallees": 1,
                    "inlineMethods": 0
                }],
                "droppedEvents": 0,
                "truncated": false
            })
        );
    }

    #[test]
    fn baked_inline_candidate_does_not_claim_backend_acceptance() {
        let value = serde_json::to_value(JitDebugEvent::InlineCandidate {
            caller_function_id: 3,
            instruction_pc: 9,
            tier: JitDebugTier::Optimizing,
            callee_function_id: Some(4),
            bake_rejection: None,
        })
        .expect("serialize inline candidate");

        assert_eq!(value["type"], "inlineCandidate");
        assert_eq!(value["callerFunctionId"], 3);
        assert_eq!(value["calleeFunctionId"], 4);
        assert!(value["bakeRejection"].is_null());
    }

    #[test]
    fn full_capture_drops_without_building_more_payloads() {
        let mut state = JitDebugState::new(JitDebugRequest::events());
        state.events = Some(vec![sample_event(); JIT_DEBUG_EVENT_LIMIT]);
        let called = Cell::new(false);

        state.record(|| {
            called.set(true);
            sample_event()
        });

        assert!(!called.get());
        let report = state.take_report().expect("capture is enabled");
        assert_eq!(report.events().len(), JIT_DEBUG_EVENT_LIMIT);
        assert_eq!(report.dropped_events(), 1);
        assert!(report.truncated());
    }

    #[test]
    fn public_report_construction_and_merge_preserve_the_hard_cap() {
        let mut oversized = Vec::with_capacity(JIT_DEBUG_EVENT_LIMIT * 4);
        oversized.extend(vec![sample_event(); JIT_DEBUG_EVENT_LIMIT + 2]);
        let report = JitDebugReport::from_events(oversized);
        assert_eq!(report.events().len(), JIT_DEBUG_EVENT_LIMIT);
        assert_eq!(report.events.capacity(), JIT_DEBUG_EVENT_LIMIT);
        assert_eq!(report.dropped_events(), 2);

        let merged = report.merged(JitDebugReport::from_events(vec![sample_event()]));
        assert_eq!(merged.events().len(), JIT_DEBUG_EVENT_LIMIT);
        assert_eq!(merged.dropped_events(), 3);
        assert!(merged.truncated());
    }

    #[test]
    fn disabling_state_discards_pending_events_without_building_more() {
        let mut state = JitDebugState::new(JitDebugRequest::events());
        state.record(sample_event);
        state.set_request(JitDebugRequest::disabled());

        let called = Cell::new(false);
        state.record(|| {
            called.set(true);
            sample_event()
        });

        assert!(!called.get());
        assert!(state.take_report().is_none());
    }
}
