//! Monotone source-function context identity for closure specialization.
//!
//! # Contents
//! - [`ClosureContexts`] owns permanent cells and generation subscriptions.
//! - Birth observation and a weak collector-rewritten first-context word.
//!
//! # Invariants
//! - Every eligible source is observed from its first closure birth. A lazy
//!   record, whose body is not compiled yet, is tracked: its first births
//!   precede the body that decides; a compiled body is classified by its
//!   own code, not by the record's placeholder.
//! - A source progresses from unseen to one context to permanently unavailable.
//! - A second context releases the first and fires every subscribed generation.
//!   Code reclamation also releases candidate sources before its full census.
//! - Native allocation accepts an identical context or a many cell; every
//!   other birth misses before effects into the common VM construction owner.
//! - Cells survive generated code, but their observed contexts are weak.
//!   Compilation and installed generations retain their own strong context
//!   constants before using a fact, so collection cannot break an active
//!   generation's dependency.
//! - Losing an unowned context permanently disables its fact before the
//!   collector reuses the dead context's offset.
//!
//! # See also
//! - [`crate::function_ops`] observes the initialized, rewritten context.
//! - [`crate::jit_registry`] verifies facts and owns their subscriptions.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Value;
use crate::code_dependencies::DependencyFlag;

#[repr(C)]
struct ContextFact {
    context: AtomicU64,
    subscribers: Vec<Arc<DependencyFlag>>,
}

impl ContextFact {
    const UNSEEN: u64 = u64::MAX;

    fn single(&self) -> Option<Value> {
        let bits = self.context.load(Ordering::Relaxed);
        (bits != Self::UNSEEN && bits != Value::undefined().to_bits())
            .then(|| Value::from_bits(bits))
    }

    fn observe(&mut self, context: Value) {
        let previous = self.context.load(Ordering::Relaxed);
        let bits = context.to_bits();
        if previous == Value::undefined().to_bits() || previous == bits {
            return;
        }
        if previous == Self::UNSEEN && context.as_context().is_some() {
            self.context.store(bits, Ordering::Relaxed);
            return;
        }
        self.invalidate();
    }

    fn invalidate(&mut self) {
        self.context
            .store(Value::undefined().to_bits(), Ordering::Relaxed);
        for flag in self.subscribers.drain(..) {
            flag.set();
        }
    }

    fn process_weak(
        &self,
        visitor: &mut dyn FnMut(otter_gc::raw::RawGc) -> Option<otter_gc::raw::RawGc>,
    ) {
        let Some(value) = self.single() else {
            return;
        };
        let raw = value.as_raw_gc().expect("single context is a GC value");
        if let Some(moved) = visitor(raw) {
            self.context.store(
                (value.to_bits() & !u64::from(u32::MAX)) | u64::from(moved.0),
                Ordering::Relaxed,
            );
        } else {
            self.context
                .store(Value::undefined().to_bits(), Ordering::Relaxed);
            for flag in &self.subscribers {
                flag.set();
            }
        }
    }
}

/// Isolate-owned facts; a missing map entry is a source not yet classified.
#[derive(Default)]
pub(crate) struct ClosureContexts {
    facts: rustc_hash::FxHashMap<u32, Option<Box<ContextFact>>>,
}

impl ClosureContexts {
    /// Classify without materializing the executable body or changing linking.
    pub(crate) fn prepare(&mut self, function: &otter_bytecode::Function) -> u64 {
        let fact = self.facts.entry(function.id).or_insert_with(|| {
            let eligible = function
                .code
                .iter()
                .any(|instruction| instruction.op == otter_bytecode::Op::LoadClosureContext)
                && function.code.iter().any(|instruction| {
                    matches!(
                        instruction.op,
                        otter_bytecode::Op::LoadContextSlot
                            | otter_bytecode::Op::LoadContextSlotChecked
                    )
                });
            eligible.then(|| {
                Box::new(ContextFact {
                    context: AtomicU64::new(ContextFact::UNSEEN),
                    subscribers: Vec::new(),
                })
            })
        });
        fact.as_ref()
            .filter(|fact| fact.context.load(Ordering::Relaxed) != Value::undefined().to_bits())
            .map_or(0, |fact| fact.context.as_ptr() as u64)
    }

    /// The fact cell of an already classified source: `Some(0)` when it is
    /// untracked or released, `None` before its first classification.
    pub(crate) fn classified(&self, function_id: u32) -> Option<u64> {
        let fact = self.facts.get(&function_id)?;
        Some(
            fact.as_ref()
                .filter(|fact| fact.context.load(Ordering::Relaxed) != Value::undefined().to_bits())
                .map_or(0, |fact| fact.context.as_ptr() as u64),
        )
    }

    /// Track a source whose body is not compiled yet: the body that decides
    /// eligibility does not exist, and its first births precede it.
    pub(crate) fn prepare_pending(&mut self, function_id: u32) -> u64 {
        let fact = self.facts.entry(function_id).or_insert_with(|| {
            Some(Box::new(ContextFact {
                context: AtomicU64::new(ContextFact::UNSEEN),
                subscribers: Vec::new(),
            }))
        });
        fact.as_ref()
            .filter(|fact| fact.context.load(Ordering::Relaxed) != Value::undefined().to_bits())
            .map_or(0, |fact| fact.context.as_ptr() as u64)
    }

    pub(crate) fn observe(&mut self, function_id: u32, context: Value) {
        if let Some(Some(fact)) = self.facts.get_mut(&function_id) {
            fact.observe(context);
        }
    }

    pub(crate) fn single(&self, function_id: u32) -> Option<(u64, Value)> {
        let fact = self.facts.get(&function_id)?.as_ref()?;
        Some((fact.context.as_ptr() as u64, fact.single()?))
    }

    pub(crate) fn subscribe(&mut self, function_id: u32, flag: &Arc<DependencyFlag>) {
        let fact = self
            .facts
            .get_mut(&function_id)
            .and_then(Option::as_mut)
            .expect("verified source context fact");
        debug_assert!(fact.single().is_some());
        fact.subscribers.push(Arc::clone(flag));
    }

    pub(crate) fn unsubscribe(&mut self, function_id: u32, flag: &Arc<DependencyFlag>) {
        if let Some(Some(fact)) = self.facts.get_mut(&function_id) {
            fact.subscribers
                .retain(|candidate| !Arc::ptr_eq(candidate, flag));
        }
    }

    /// Drop optimization-owned roots before a candidate chunk's GC census.
    pub(crate) fn release_range(&mut self, start: u32, end: u32) -> bool {
        let mut released = false;
        for (&function_id, fact) in &mut self.facts {
            if function_id >= start
                && function_id < end
                && let Some(fact) = fact
                && fact.single().is_some()
            {
                fact.invalidate();
                released = true;
            }
        }
        released
    }

    /// Keep a fact only while a non-speculative owner keeps its context alive.
    pub(crate) fn process_weak(
        &self,
        visitor: &mut dyn FnMut(otter_gc::raw::RawGc) -> Option<otter_gc::raw::RawGc>,
    ) {
        for fact in self.facts.values().flatten() {
            fact.process_weak(visitor);
        }
    }
}

impl crate::Interpreter {
    pub(crate) fn note_closure_context(
        &mut self,
        source: &crate::ExecutionContext,
        closure: crate::closure::JsClosure,
    ) {
        let function_id = closure.function_id();
        if self.prepare_closure_context(source, function_id) != 0 {
            self.jit_code_registry
                .closure_contexts
                .observe(function_id, closure.context(&self.gc_heap));
        }
    }

    pub(crate) fn prepare_closure_context(
        &mut self,
        source: &crate::ExecutionContext,
        function_id: u32,
    ) -> u64 {
        if let Some(cell) = self
            .jit_code_registry
            .closure_contexts
            .classified(function_id)
        {
            return cell;
        }
        if source.lazy_pending(function_id) {
            return self
                .jit_code_registry
                .closure_contexts
                .prepare_pending(function_id);
        }
        let Ok(owner) = source.for_function(function_id) else {
            return 0;
        };
        owner.function(function_id).map_or(0, |function| {
            self.jit_code_registry.closure_contexts.prepare(function)
        })
    }
}

#[cfg(test)]
#[path = "closure_contexts_tests.rs"]
mod tests;
