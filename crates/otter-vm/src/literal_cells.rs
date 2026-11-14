//! Stable literal cells and their remaining young-generation roots.
//!
//! # Contents
//! - [`LiteralCells`] owns the cells shared by bytecode and native literal loads.
//! - A mutator-only frontier excludes promoted literals from minor root walks.
//! - Source eviction removes frontier keys before freeing their stable cells.
//!
//! # Invariants
//! - Production cells contain immutable String or BigInt values and are inserted
//!   once. Only moving GC rewrites a published value; its box address never moves.
//! - Compiler threads read only the map under `MutatorOwned` and the heap-read
//!   right. Every frontier access requires the isolate's `MutatorToken` or an
//!   exclusive owner borrow; no compiler thread can borrow its `RefCell`.
//! - Preparation examines current rooted handles before forwarding begins. A
//!   removed old value cannot become young again; All walks retain every cell.
//!
//! # See also
//! - `crate::compile_shared` owns the compiler/mutator sharing boundary.
//! - `crate::interp::jit_bake` embeds cell addresses into native relocations.

use crate::gc_trace::GcRootVisitor;
use crate::mutator_owned::MutatorToken;
use crate::{Interpreter, Value};
use otter_gc::RootSet;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::collections::hash_map::Entry;

type LiteralKey = (usize, u32);

#[derive(Default)]
pub(crate) struct LiteralCells {
    cells: FxHashMap<LiteralKey, Box<Value>>,
    young: RefCell<Vec<LiteralKey>>,
}

impl LiteralCells {
    #[inline]
    pub(crate) fn get(&self, key: &LiteralKey) -> Option<&Value> {
        self.cells.get(key).map(Box::as_ref)
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &Value> {
        self.cells.values().map(Box::as_ref)
    }

    /// Canonicalize a successful cold materialization without replacing a
    /// cell address that a compiled generation may already reference.
    pub(crate) fn insert(&mut self, key: LiteralKey, value: Value) {
        let Entry::Vacant(entry) = self.cells.entry(key) else {
            panic!("literal cells canonicalize once");
        };
        self.young.get_mut().push(key);
        entry.insert(Box::new(value));
    }

    pub(crate) fn remove_sources(&mut self, identities: &FxHashSet<usize>) {
        self.young
            .get_mut()
            .retain(|(identity, _)| !identities.contains(identity));
        self.cells
            .retain(|(identity, _), _| !identities.contains(identity));
    }

    pub(crate) fn prepare_collection(&self, _mutator: &MutatorToken) {
        self.young.borrow_mut().retain(|key| {
            let value = self
                .cells
                .get(key)
                .expect("frontier owns a live literal cell");
            let Some(raw) = value.as_raw_gc() else {
                return false;
            };
            // SAFETY: this handle was read from a strongly rooted cell. The
            // preparation callback runs before any forwarding or sweeping,
            // excludes compiler heap readers, and never allocates or reenters
            // JavaScript. The header bit also covers young-large promotion,
            // which changes generation without changing the handle offset.
            unsafe { (*raw.as_header_ptr()).is_young() }
        });
    }

    fn trace_roots(
        &self,
        roots: RootSet,
        _mutator: &MutatorToken,
        visitor: &mut GcRootVisitor<'_>,
    ) {
        match roots {
            RootSet::All => {
                for value in self.values() {
                    value.trace_value_slots(visitor);
                }
            }
            RootSet::Young => {
                for key in self.young.borrow().iter() {
                    self.cells
                        .get(key)
                        .expect("frontier owns a live literal cell")
                        .trace_value_slots(visitor);
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.cells.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains_key(&self, key: &LiteralKey) -> bool {
        self.cells.contains_key(key)
    }

    #[cfg(test)]
    pub(crate) fn keys(&self) -> impl Iterator<Item = &LiteralKey> {
        self.cells.keys()
    }

    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&LiteralKey, &Value)> {
        self.cells.iter().map(|(key, value)| (key, value.as_ref()))
    }

    #[cfg(test)]
    pub(crate) fn clear_value_for_test(&mut self, key: &LiteralKey) {
        **self.cells.get_mut(key).expect("prepared literal cell") = Value::hole();
    }

    #[cfg(test)]
    pub(crate) fn queued_keys_for_test(&self, _mutator: &MutatorToken) -> Vec<LiteralKey> {
        self.young.borrow().clone()
    }
}

impl Interpreter {
    pub(crate) fn trace_literal_cells(&self, roots: RootSet, visitor: &mut GcRootVisitor<'_>) {
        self.literal_cells()
            .trace_roots(roots, &self.mutator, visitor);
    }
}
