//! Function-id liveness census for reclaimable code chunks.
//!
//! Code values store dense `u32` ids rather than owning their linked chunk.
//! At a between-turn safepoint this module walks every live GC payload that can
//! carry such an id and the isolate-owned queues/root stores outside the heap.
//! Candidate ranges are allocated before the walk; the visitor only flips
//! pre-existing bits and never allocates.
//!
//! # Contents
//!
//! - [`CandidateLiveness`] — allocation-free marking over selected ranges.
//! - [`census_candidate_ids`] — heap and isolate-root census entry point.
//! - [`visit_value`] — shared immediate-function-id decoder.
//!
//! # Invariants
//!
//! - The caller has completed a full collection and runs between JavaScript
//!   turns with no materialized or native activation.
//! - Every candidate range is disjoint and ranges are sorted by base before a
//!   visitor runs.
//! - Census callbacks do not allocate, execute JavaScript, or mutate the heap.
//! - Retained [`crate::ExecutionContext`] ownership is proved separately by
//!   the code-space payload `Arc`; timers, realm contexts, parked dynamic
//!   imports, and other queued work therefore block physical reclamation even
//!   when they carry no bare id visible to this visitor.
//! - JIT entries, feedback, function-keyed caches, and diagnostic frame ids are
//!   non-owning metadata. Eviction invalidates or purges the executable tables
//!   before tombstoning; diagnostics retain copied names and spans rather than
//!   keeping source payloads alive.
//!
//! # See also
//!
//! - [`crate::code_space`]
//! - [`crate::native_census`]

use crate::code_space::ChunkEvictionCandidate;

/// Liveness bits for a preallocated, base-sorted candidate set.
#[derive(Debug)]
pub(crate) struct CandidateLiveness {
    candidates: Vec<ChunkEvictionCandidate>,
    live: Vec<bool>,
}

impl CandidateLiveness {
    #[must_use]
    pub(crate) fn new(mut candidates: Vec<ChunkEvictionCandidate>) -> Self {
        candidates.sort_unstable_by_key(|candidate| candidate.function_base);
        let live = vec![false; candidates.len()];
        Self { candidates, live }
    }

    pub(crate) fn mark(&mut self, function_id: u32) {
        let index = self
            .candidates
            .partition_point(|candidate| candidate.function_base <= function_id);
        let Some(index) = index.checked_sub(1) else {
            return;
        };
        let candidate = self.candidates[index];
        if function_id < candidate.function_end() {
            self.live[index] = true;
        }
    }

    #[must_use]
    pub(crate) fn is_live(&self, candidate: ChunkEvictionCandidate) -> bool {
        self.candidates
            .binary_search_by_key(&candidate.function_base, |entry| entry.function_base)
            .ok()
            .is_none_or(|index| self.live[index])
    }
}

/// Mark an immediate bytecode-function value. Heap-backed closures are visited
/// through their [`crate::closure::JsClosureBody`] payload.
#[inline]
pub(crate) fn visit_value(value: &crate::Value, visitor: &mut dyn FnMut(u32)) {
    if let Some(function_id) = value.as_function() {
        visitor(function_id);
    }
}

pub(crate) fn visit_descriptor(
    descriptor: &crate::object::PropertyDescriptor,
    visitor: &mut dyn FnMut(u32),
) {
    match &descriptor.kind {
        crate::object::DescriptorKind::Data { value } => visit_value(value, visitor),
        crate::object::DescriptorKind::Accessor { getter, setter } => {
            if let Some(value) = getter {
                visit_value(value, visitor);
            }
            if let Some(value) = setter {
                visit_value(value, visitor);
            }
        }
    }
}

/// Walk every live bare function id relevant to `candidates`.
#[must_use]
pub(crate) fn census_candidate_ids(
    interpreter: &crate::Interpreter,
    candidates: Vec<ChunkEvictionCandidate>,
) -> CandidateLiveness {
    let mut liveness = CandidateLiveness::new(candidates);
    {
        let mut visit = |function_id| liveness.mark(function_id);
        let heap = &interpreter.gc_heap;

        heap.for_each_live_payload::<crate::value_slab::ValueSlabBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::array::elements::ElementSlabBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::object::slot_slab::SlotSlabBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::object::ObjectBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::object::ExoticSlots, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::object::SymbolPropsBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::object::AccessorCellBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::array::ArrayExoticSlots, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::collections::table::OrderedTableBody<
        crate::collections::MapEntry,
    >, _>(|_, body| body.visit_function_ids(&mut visit));
        heap.for_each_live_payload::<crate::collections::table::OrderedTableBody<
        crate::collections::SetEntry,
    >, _>(|_, body| body.visit_function_ids(&mut visit));
        heap.for_each_live_payload::<crate::collections::MapBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::collections::SetBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::collections::WeakMapBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::collections::WeakSetBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::collections::weak_table::WeakTableBody<
        crate::collections::weak_table::MapKind,
    >, _>(|_, body| body.visit_function_ids(&mut visit));
        heap.for_each_live_payload::<crate::collections::weak_table::WeakTableBody<
        crate::collections::weak_table::SetKind,
    >, _>(|_, body| body.visit_function_ids(&mut visit));
        heap.for_each_live_payload::<crate::upvalue::UpvalueCellBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::closure::JsClosureBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::bound_function::BoundFunctionBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::class_constructor::ClassConstructorBody, _>(
            |_, body| {
                body.visit_function_ids(&mut visit);
            },
        );
        heap.for_each_live_payload::<crate::generator::GeneratorBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::generator::ParkedFrameBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::promise::PurePromiseBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::proxy::ProxyBodyGc, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::proxy::PrivateSlotsBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::iterator_state::IteratorState, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::native_function::NativeFunctionBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::regexp::JsRegExpBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::weak_refs::WeakRefBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::weak_refs::FinalizationRegistryBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::binary::array_buffer::LocalArrayBufferBodyGc, _>(
            |_, body| body.visit_function_ids(&mut visit),
        );
        heap.for_each_live_payload::<crate::binary::array_buffer::SharedArrayBufferBodyGc, _>(
            |_, body| body.visit_function_ids(&mut visit),
        );
        heap.for_each_live_payload::<crate::binary::data_view::DataViewBodyGc, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::binary::typed_array::TypedArrayBodyGc, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });
        heap.for_each_live_payload::<crate::intl::payload::IntlBody, _>(|_, body| {
            body.visit_function_ids(&mut visit);
        });

        interpreter.visit_root_function_ids(&mut visit);
    }
    liveness
}
