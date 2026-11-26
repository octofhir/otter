//! Mutator-side preparation and validation of JIT compile facts.
//!
//! # Contents
//! - Literal and prototype preparation, context watches, and post-bake install tasks.
//!
//! # Invariants
//! - Baking reads prepared facts. The isolate checks deferred watches before publishing code.
//!
//! # See also
//! - [`super::jit_bake`] captures facts for the compiler.

use super::jit_bake::literal_matches;
use crate::*;

impl Interpreter {
    /// Make the facts a compilation of `fid` reads exist: canonical
    /// literals, the realm's Array chain and iteration proofs, collection
    /// prototypes in their final hidden classes, the intrinsic programs of
    /// its named sites, the global object's layout proof, the classification
    /// of its closures' contexts and the watch of its own closure context.
    /// Each step is cached and idempotent; baking reads what they made.
    pub(super) fn prepare_compile_facts(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        if self.jit_retraining_blocks(fid) {
            return None;
        }
        let literals = self.prewarm_literal_cells(context, fid);
        self.prewarm_array_prototype_chain();
        // Prototypes first: the intrinsic programs' layout stamp names their
        // final shapes.
        self.prepare_intrinsic_prototypes();
        let owner = context.for_function(fid).ok()?;
        let function = owner.exec_function(fid)?;
        let mut iterates = false;
        for instruction in function.code.iter() {
            match function.op(instruction) {
                Op::LoadProperty | Op::HasNamedProperty | Op::CallMethodValue => {
                    if let Some(key) = function
                        .const_index(instruction, 2)
                        .and_then(|name| owner.property_atom_for_function(fid, name))
                    {
                        self.prepare_intrinsic_property_programs(key);
                    }
                }
                Op::MakeClosure | Op::MakeFunction => {
                    if let Some(function_id) = function
                        .const_index(instruction, 1)
                        .and_then(|index| owner.function_id_constant(index))
                    {
                        self.prepare_closure_context(&owner, function_id);
                    }
                }
                Op::GetIterator | Op::IteratorClose | Op::IteratorCloseThrow => iterates = true,
                _ => {}
            }
        }
        if iterates {
            let _ = self.array_iteration_cells();
        }
        let _ = crate::object::dictionary_layout_validity(self.global_this, &self.gc_heap);
        if let Some((_, closure_context)) = self.jit_code_registry.closure_contexts.single(fid)
            && let Some(closure_context) = closure_context.as_context()
        {
            let mut current = Some(closure_context);
            while let Some(context) = current {
                let _ = self
                    .jit_code_registry
                    .ensure_context_watch(&mut self.gc_heap, context);
                current = crate::context::walk(&self.gc_heap, context, 1);
            }
        }
        literals
    }

    /// Run a compilation's install tasks before its code is published; `false`
    /// when a fact its code reads changed since baking, which declines it.
    pub(super) fn run_install_tasks(&mut self, install: super::jit_bake::InstallTasks) -> bool {
        let super::jit_bake::InstallTasks {
            instanceof_cells,
            global_slot_watches,
            context_slot_watches,
        } = install;
        for watch in &global_slot_watches {
            let owns = matches!(
                crate::object::lookup_own_slot(self.global_this, &self.gc_heap, &watch.name),
                (Some(hit), crate::object::PropertyLookup::Data { .. }) if hit.slot == watch.slot
            );
            if !owns
                || crate::object::dictionary_layout(self.global_this, &self.gc_heap)
                    != Some(watch.layout)
                || !crate::object::watch_dictionary_slot(
                    self.global_this,
                    &mut self.gc_heap,
                    watch.slot,
                )
            {
                return false;
            }
        }
        for watch in &context_slot_watches {
            let Some(context) = watch.context.held().as_context() else {
                return false;
            };
            let expected = watch
                .value_cell
                .as_ref()
                .map_or(watch.bits, |cell| cell.held().to_bits());
            let Some(current) = crate::context::read_slot(&self.gc_heap, context, watch.slot)
            else {
                return false;
            };
            let watched =
                self.jit_code_registry
                    .watch_context_slot(&mut self.gc_heap, context, watch.slot);
            if current.to_bits() != expected {
                // Written after baking read it: the slot is volatile as if
                // its watch had fired, so no later compile folds it.
                if watched.is_some() {
                    crate::code_dependencies::context_slot_written(
                        crate::context::watch(&self.gc_heap, context),
                        watch.slot,
                    );
                }
                return false;
            }
            if watched != Some(watch.watch) {
                return false;
            }
        }
        if !instanceof_cells.is_empty() {
            let mut cells = self.jit_instanceof_cells.borrow_mut();
            for cell in instanceof_cells {
                if cells.len().is_power_of_two() {
                    cells.retain(|cell| cell.strong_count() != 0);
                }
                cells.push(std::sync::Arc::downgrade(&cell));
            }
        }
        true
    }

    /// Give the default realm's %Array.prototype% chain the prototype role
    /// whose writes retire proofs, once, so a bake reads that chain's validity
    /// cell for an Array method call without allocating. Like the literal
    /// prewarm, it runs before a body's snapshot and may collect.
    pub(super) fn prewarm_array_prototype_chain(&mut self) {
        if self.active_realm_id != 0 || !self.gc_heap.has_frame_root_providers() {
            return;
        }
        let Some(prototype) = self.default_array_prototype() else {
            return;
        };
        if crate::object::prototype_validity::chain_validity(prototype, &self.gc_heap).is_none() {
            let _ = self.prove_chain(Self::default_array_prototype);
        }
    }

    /// Canonicalize every string literal needed by `fid` before snapshotting.
    ///
    /// Existing canonical cells and functions without literals are
    /// allocation-free and need no active frame-root provider. The first cold
    /// literal may allocate and therefore requires the current activation stack
    /// to be registered with the collector. Allocation failure only declines
    /// this optional compile; it does not publish a pending JavaScript throw or
    /// retain an error for later execution.
    pub(super) fn prewarm_literal_cells(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        if self.jit_retraining_blocks(fid) {
            return None;
        }
        let owner = context.for_function(fid).ok()?;
        let function = owner.exec_function(fid)?;
        let mut instruction_index = 0usize;
        while let Some(instruction) = function.instr_at_index(instruction_index) {
            instruction_index = instruction_index.checked_add(1)?;
            let op = function.op(instruction);
            if !matches!(op, Op::LoadString | Op::LoadBigInt) {
                continue;
            }
            let constant = function.const_index(instruction, 1)?;
            let key = owner.constant_cache_key(constant);
            if let Some(cell) = self.literal_cells().get(&key) {
                if !literal_matches(op, cell) {
                    return None;
                }
                continue;
            }
            if !self.gc_heap.has_frame_root_providers() {
                return None;
            }
            let value = if op == Op::LoadString {
                self.load_string_constant_value(&owner, constant)
            } else {
                self.load_bigint_constant_value(&owner, constant)
            };
            debug_assert!(value.as_ref().is_ok_and(|value| literal_matches(op, value)));
            value.ok()?;
        }
        Some(())
    }
}
