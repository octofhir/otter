//! Method-call feedback and invalidation of cached instanceof proofs.
//!
//! # Contents
//! - Receiver and holder classification, method targets and native leaf eligibility.
//!
//! # Invariants
//! - Saturated call sites retain their megamorphic state. Closure mutation invalidates cached instanceof proofs.
//!
//! # See also
//! - [`super::jit_bake`] consumes the recorded feedback.

use crate::*;

impl Interpreter {
    /// Empty every generated `instanceof` cell before `closure` changes its
    /// `prototype`, own properties or `[[Prototype]]`, when a cell ever
    /// cached it: no site may answer from a pair the change invalidates.
    pub(crate) fn retire_instanceof_proofs_for(&self, closure: crate::closure::JsClosure) {
        if !closure.instanceof_cached(&self.gc_heap) {
            return;
        }
        self.jit_instanceof_cells.borrow_mut().retain(|cell| {
            let Some(cell) = cell.upgrade() else {
                return false;
            };
            cell.clear();
            true
        });
    }

    /// Whether a method-call site's feedback has already saturated to
    /// `Megamorphic`. Once it has, further [`Self::note_method_target`]
    /// observations are no-ops, so a caller can skip the receiver/prototype
    /// shape walk that only exists to build the `MethodSite` argument — the hot
    /// path for a megamorphic site (e.g. one `arr[i].run()` over many classes).
    pub(crate) fn method_site_feedback_saturated(&self, site: usize) -> bool {
        self.method_target_feedback_saturated(site)
    }

    /// Record the live `Mono`/`Poly` overlay for one `Op::CallMethodValue` site.
    ///
    /// Returns `true` when the bounded target population changes. The caller
    /// uses that transition to invalidate an immutable optimizing guard chain;
    /// repeated hits merely update frequency and return `false`.
    pub(crate) fn note_method_target(
        &mut self,
        feedback_site: usize,
        method_fid: u32,
        site: MethodSite,
    ) -> bool {
        self.record_method_target_feedback(feedback_site, method_fid, site)
    }

    /// Classify the method a `CallMethodValue` site is about to invoke against
    /// the declared leaf-callable builtin table.
    ///
    /// Runs only on the feedback-capture path, which is already gated on an
    /// unsaturated site, and reads the method through the non-observable own /
    /// prototype data lookup. `None` whenever the callee is not a declared
    /// entry or the site's argument count is not the one that entry implements
    /// — the same declaration gate the backend applies, checked once here so a
    /// mismatched site never records.
    pub(crate) fn method_slot_native_leaf(
        &self,
        context: &ExecutionContext,
        caller_fid: u32,
        name_idx: u32,
        argc: usize,
        recv: Value,
    ) -> Option<crate::native_abi::RuntimeStubId> {
        let name = context.property_atom_for_function(caller_fid, name_idx)?;
        let method = crate::object::get(recv.as_object()?, &self.gc_heap, name.name())?;
        let declaration = crate::jit_static_native::jit_static_call_target(
            method.as_native_function()?,
            &self.gc_heap,
        )?;
        // A shaped method site passes only its arguments, so an entry that
        // reads `this` as an operand word is never recorded here.
        (argc == usize::from(declaration.argument_count) && !declaration.this_operand)
            .then_some(declaration.leaf_stub_id)
    }

    pub(crate) fn method_site_for_receiver(
        &mut self,
        context: &ExecutionContext,
        caller_fid: u32,
        name_idx: u32,
        recv: &mut Value,
    ) -> Option<MethodSite> {
        let name = context.property_atom_for_function(caller_fid, name_idx)?;
        let mut receiver = recv.as_object()?;
        // A receiver without a hidden class cannot be named by any guard, so
        // put it and its prototype chain on the shaped path first. The
        // migration allocates and may relocate the receiver, so the caller's
        // value is refreshed from the rooted handle before anything reads it.
        self.migrate_slow_to_fast(&mut receiver);
        *recv = Value::object(receiver);
        let recv = receiver;
        let recv_shape_handle = crate::object::keyed_shape(recv, &self.gc_heap);
        if recv_shape_handle.is_null() {
            return None;
        }
        let recv_shape = crate::object::shape_id(recv, &self.gc_heap);
        let resolved = crate::cache_ir::resolve_atom_data_slot(recv, &self.gc_heap, name)?;
        // A wide dictionary holder can keep ordinary data semantics without
        // having a keyed layout that native method linkage can guard. The
        // receiver's shape does not describe that holder's storage bank.
        if resolved.hit.shape.is_null() {
            return None;
        }
        self.shape_runtime
            .register_shape(&self.gc_heap, recv_shape_handle);
        if !resolved.holder_root.is_null() {
            self.shape_runtime
                .register_shape(&self.gc_heap, resolved.holder_root);
        }
        Some(MethodSite {
            recv_shape,
            prototype: crate::MethodLookupProof {
                validity: resolved.validity,
                holder_root: resolved.holder_root_id,
            },
            method_field: crate::object::field_location(
                resolved.hit.shape,
                u32::from(resolved.hit.slot),
            ),
        })
    }
}
