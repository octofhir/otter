//! Monotone source-function context identity for closure specialization.
//!
//! # Contents
//! - [`ClosureContexts`] owns permanent cells and generation subscriptions.
//! - Birth observation and a collector-rewritten first-context word.
//!
//! # Invariants
//! - Every eligible source is observed from its first closure birth.
//! - A source progresses from unseen to one context to permanently unavailable.
//! - A second context releases the first and fires every subscribed generation.
//!   Code reclamation also releases candidate sources before its full census.
//! - Native allocation accepts an identical context or a many cell; every
//!   other birth misses before effects into the common VM construction owner.
//! - Cells survive generated code and are traced at every moving collection.
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

    pub(crate) fn trace(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for fact in self.facts.values().flatten() {
            if fact.single().is_some() {
                visitor(fact.context.as_ptr().cast());
            }
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
        if let Some(function) = source.function(function_id) {
            self.jit_code_registry.closure_contexts.prepare(function);
            self.jit_code_registry
                .closure_contexts
                .observe(function_id, closure.context(&self.gc_heap));
        }
    }
}

#[cfg(test)]
#[path = "closure_contexts_tests.rs"]
mod tests;
