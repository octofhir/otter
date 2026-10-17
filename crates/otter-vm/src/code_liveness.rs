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
//! - The heap is walked once: each live body is dispatched by its type tag.
//! - Retained [`crate::ExecutionContext`] ownership is proved separately by
//!   the code-space payload `Arc`; timers, realm contexts, parked dynamic
//!   imports, and other queued work therefore block physical reclamation even
//!   when they carry no bare id visible to this visitor.
//! - A live context reports its descriptor-owning function id, so the scope
//!   table that names its slots outlives every context of the chunk.
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

    /// Keep every lazy body alive whose creating chunk is: live as a
    /// candidate, or not a candidate at all and still linked (`linked`).
    ///
    /// A body's function 0 runs under its record's id, so closures and
    /// contexts of it mark the creating chunk, not the body. A record's id
    /// lies below its body's range, so one pass in base order sees each
    /// creator settled before its bodies.
    pub(crate) fn keep_bodies_of_live_creators(&mut self, linked: impl Fn(u32) -> bool) {
        for index in 0..self.candidates.len() {
            let Some(alias) = self.candidates[index].alias else {
                continue;
            };
            let creator = self
                .candidates
                .partition_point(|candidate| candidate.function_base <= alias)
                .checked_sub(1)
                .filter(|&creator| alias < self.candidates[creator].function_end());
            let creator_live = match creator {
                Some(creator) => self.live[creator],
                None => linked(alias),
            };
            if creator_live {
                self.live[index] = true;
            }
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
        // One walk of the pages; every body that can carry a function id is
        // dispatched by its type tag.
        interpreter.gc_heap.for_each_live_body(|_, body| {
            macro_rules! dispatch {
                ($($body:ty),* $(,)?) => {
                    $(
                        if let Some(body) = body.downcast::<$body>() {
                            body.visit_function_ids(&mut visit);
                            return;
                        }
                    )*
                };
            }
            dispatch!(
                crate::object::ObjectBody,
                crate::object::slot_slab::SlotSlabBody,
                crate::value_slab::ValueSlabBody,
                crate::array::elements::ElementSlabBody,
                crate::object::ShapeBody,
                crate::closure::JsClosureBody,
                crate::context::ContextBody,
                crate::object::ExoticSlots,
                crate::object::symbol_table::SymbolPropsBody,
                crate::object::AccessorCellBody,
                crate::closure_construct::ClosureRareBody,
                crate::constructor_layout::ConstructorLayoutBody,
                crate::array::ArrayExoticSlots,
                crate::collections::table::OrderedTableBody<crate::collections::MapEntry>,
                crate::collections::table::OrderedTableBody<crate::collections::SetEntry>,
                crate::collections::MapBody,
                crate::collections::SetBody,
                crate::collections::WeakMapBody,
                crate::collections::WeakSetBody,
                crate::collections::weak_table::WeakTableBody<
                    crate::collections::weak_table::MapKind,
                >,
                crate::collections::weak_table::WeakTableBody<
                    crate::collections::weak_table::SetKind,
                >,
                crate::upvalue::UpvalueCellBody,
                crate::eval_env::EvalExtensionBody,
                crate::bound_function::BoundFunctionBody,
                crate::class_constructor::ClassConstructorBody,
                crate::generator::GeneratorBody,
                crate::generator::ParkedFrameBody,
                crate::promise::PurePromiseBody,
                crate::proxy::ProxyBodyGc,
                crate::proxy::PrivateSlotsBody,
                crate::iterator_state::IteratorState,
                crate::native_function::NativeFunctionBody,
                crate::regexp::JsRegExpBody,
                crate::weak_refs::WeakRefBody,
                crate::weak_refs::FinalizationRegistryBody,
                crate::binary::array_buffer::LocalArrayBufferBodyGc,
                crate::binary::array_buffer::SharedArrayBufferBodyGc,
                crate::binary::data_view::DataViewBodyGc,
                crate::binary::typed_array::TypedArrayBodyGc,
                crate::intl::payload::IntlBody,
            );
        });

        interpreter.visit_root_function_ids(&mut visit);
    }
    liveness.keep_bodies_of_live_creators(|function_id| {
        matches!(
            interpreter.code_space.resolve_chunk(function_id),
            crate::code_space::ChunkResolution::Live { .. }
        )
    });
    liveness
}
