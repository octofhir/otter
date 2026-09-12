//! Instance-capacity observations used by constructor receiver preparation.
//!
//! # Contents
//! - Bounded field-count profiles and scalar pre-allocation samples.
//! - Receiver observations, pre-collection flushing and deferred invalidation.
//!
//! # Invariants
//! - Learned capacity is bounded independently of an instance's property graph.
//! - A preparation sample contains only scalar identity and capacity; moving
//!   collection cannot leave a captured constructor handle stale.
//! - Observation handles are never roots. The collector prologue folds their
//!   sizes into scalar maxima and drops every receiver/owner before movement
//!   or sweep. Eager root snapshots and raw heap borrows flush before exposure.
//! - Only pending owners are visited; the ledger is emptied without GC
//!   allocation. Code invalidation is deferred to normal constructor entry.
//!
//! # See also
//! - `call_ops` for receiver allocation and constructor dispatch.
//! - `closure::JsClosureBody` for per-function-object observations.

use crate::{Interpreter, JsObject, Value};
use std::cell::Cell;

/// Instance size learned for one exact constructor / `new.target` pair.
///
/// The next preparation or pre-collection flush observes the latest receiver's
/// size. Only the bounded scalar maximum survives a collection, so a receiver's
/// property graph cannot become live solely because of profiling.
#[derive(Debug, Default)]
pub(crate) struct ConstructorInstanceProfile {
    /// Largest property count observed on a prepared receiver so far, across
    /// every closure of the template.
    pub(crate) learned: Cell<usize>,
    /// The most recently prepared receiver of a non-closure constructor,
    /// sampled at the next preparation. Closures keep theirs in the body.
    pub(crate) last_receiver: Cell<Option<JsObject>>,
    /// A GC observation crossed inline capacity; retire old plans at VM entry.
    pub(crate) needs_invalidation: Cell<bool>,
}

impl ConstructorInstanceProfile {
    /// Field count to reserve for the next receiver: the recorded maximum
    /// folded with the size the previous instance has grown to, bounded like
    /// JSC's maximum inline capacity so a pathological instance cannot turn
    /// every later allocation into a large slab.
    pub(crate) fn learned_field_count(&self, heap: &otter_gc::GcHeap) -> usize {
        Self::learned_from(self.learned.get(), self.last_receiver.get(), heap)
    }

    /// Fold a recorded maximum with the size a sampled instance reached.
    pub(crate) fn learned_from(
        learned: usize,
        last_receiver: Option<JsObject>,
        heap: &otter_gc::GcHeap,
    ) -> usize {
        const MAX_LEARNED_FIELDS: usize = 64;
        let sampled = last_receiver
            .map(|receiver| heap.read_payload(receiver, crate::object::ObjectBody::slab_len))
            .unwrap_or(0);
        learned.max(sampled).min(MAX_LEARNED_FIELDS)
    }
}

/// One constructor's learned instance size read before a construct, plus
/// where the receiver it produces is recorded afterwards. Contains only owned
/// scalar data, so receiver allocation cannot leave a stale moving GC handle.
pub(crate) struct ConstructorProfileSample {
    /// Function-keyed identity for the template-wide maximum.
    pub(crate) key: (u32, u32),
    /// Field count the constructor's receivers have reached so far.
    pub(crate) learned: usize,
}

/// A sampled owner that is valid only until the next pre-collection flush.
/// No entry participates in tracing or survives movement/sweep.
pub(crate) struct PendingConstructorSample {
    pub(crate) key: (u32, u32),
    pub(crate) closure: Option<otter_gc::Gc<crate::closure::JsClosureBody>>,
}

impl Interpreter {
    /// Exact constructor / `new.target` identity for a receiver profile:
    /// the base body plus the ordinary function or class constructor that
    /// `new.target` names.
    fn constructor_instance_key(&self, base_function_id: u32, new_target: Value) -> (u32, u32) {
        let callable = new_target
            .as_class_constructor()
            .map(|class| class.ctor(&self.gc_heap))
            .unwrap_or(new_target);
        let new_target_function_id = callable
            .as_function()
            .or_else(|| {
                callable
                    .as_closure(&self.gc_heap)
                    .map(|closure| closure.function_id())
            })
            .unwrap_or(base_function_id);
        (base_function_id, new_target_function_id)
    }

    /// The constructor's learned instance size before this construct: the
    /// closure's own profile when `new_target` is a closure (sibling closures
    /// of one template construct unrelated classes), otherwise the function's.
    pub(crate) fn sample_constructor_profile(
        &mut self,
        base_function_id: u32,
        new_target: Value,
    ) -> ConstructorProfileSample {
        let key = self.constructor_instance_key(base_function_id, new_target);
        if self
            .constructor_instance_profiles
            .get(&key)
            .is_some_and(|profile| profile.needs_invalidation.replace(false))
        {
            self.invalidate_jit_function(key.0);
        }
        let callable = new_target
            .as_class_constructor()
            .map(|class| class.ctor(&self.gc_heap))
            .unwrap_or(new_target);
        let closure = callable
            .as_closure(&self.gc_heap)
            .map(|closure| closure.handle);
        let learned = match closure {
            Some(handle) => {
                let (learned, last) = self.gc_heap.read_payload(handle, |body| {
                    (body.learned_instance_fields.get(), body.last_instance.get())
                });
                ConstructorInstanceProfile::learned_from(usize::from(learned), last, &self.gc_heap)
            }
            None => self
                .constructor_instance_profiles
                .get(&key)
                .map(|profile| profile.learned_field_count(&self.gc_heap))
                .unwrap_or(0),
        };
        ConstructorProfileSample { key, learned }
    }

    /// Record the receiver just prepared so the next preparation can sample
    /// the size it grows to. The function-keyed entry keeps the largest size
    /// any closure of the template reached, which decides whether an inline
    /// allocation plan may still be baked for it.
    pub(crate) fn note_constructor_receiver(
        &mut self,
        sample: ConstructorProfileSample,
        new_target: Value,
        receiver: JsObject,
    ) {
        let ConstructorProfileSample { key, learned } = sample;
        // Allocation may have moved the constructor since sampling. Resolve
        // its identity from the caller's rewritten root only after allocation.
        let callable = new_target
            .as_class_constructor()
            .map(|class| class.ctor(&self.gc_heap))
            .unwrap_or(new_target);
        let closure = callable
            .as_closure(&self.gc_heap)
            .map(|closure| closure.handle);
        if let Some(handle) = closure {
            self.gc_heap.with_payload(handle, |body| {
                body.learned_instance_fields.set(
                    body.learned_instance_fields
                        .get()
                        .max(u16::try_from(learned).unwrap_or(u16::MAX)),
                );
                if body.last_instance.replace(Some(receiver)).is_none() {
                    self.pending_constructor_samples
                        .borrow_mut()
                        .push(PendingConstructorSample {
                            key,
                            closure: Some(handle),
                        });
                }
            });
        }
        let profile = self.constructor_instance_profiles.entry(key).or_default();
        let crossed_inline_capacity = profile.needs_invalidation.replace(false)
            || (profile.learned.get() <= crate::object::INLINE_SLOT_CAP
                && learned > crate::object::INLINE_SLOT_CAP);
        profile.learned.set(profile.learned.get().max(learned));
        if closure.is_none() && profile.last_receiver.replace(Some(receiver)).is_none() {
            self.pending_constructor_samples
                .borrow_mut()
                .push(PendingConstructorSample { key, closure: None });
        }
        if crossed_inline_capacity {
            // Callers compiled against an inline allocation plan for this
            // template would keep handing out receivers without the slab the
            // body needs; retire them so they re-bake against the runtime
            // preparation.
            self.invalidate_jit_function(key.0);
        }
    }

    /// Fold pending receiver sizes into scalar profiles before any GC movement.
    /// Also used before eager out-of-band root snapshots, while offsets are
    /// still valid. Interior mutation is confined to private profiling state.
    pub(crate) fn flush_constructor_observations(&self, heap: &otter_gc::GcHeap) {
        for sample in self.pending_constructor_samples.borrow_mut().drain(..) {
            let profile = self.constructor_instance_profiles.get(&sample.key);
            let learned = if let Some(closure) = sample.closure {
                heap.read_payload(closure, |body| {
                    let learned = ConstructorInstanceProfile::learned_from(
                        usize::from(body.learned_instance_fields.get()),
                        body.last_instance.take(),
                        heap,
                    );
                    body.learned_instance_fields.set(learned as u16);
                    learned
                })
            } else {
                let Some(profile) = profile else {
                    continue;
                };
                ConstructorInstanceProfile::learned_from(
                    profile.learned.get(),
                    profile.last_receiver.take(),
                    heap,
                )
            };
            let Some(profile) = profile else {
                continue;
            };
            if profile.learned.get() <= crate::object::INLINE_SLOT_CAP
                && learned > crate::object::INLINE_SLOT_CAP
            {
                profile.needs_invalidation.set(true);
            }
            profile.learned.set(profile.learned.get().max(learned));
        }
    }
}
