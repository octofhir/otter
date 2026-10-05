//! Family-owned bytecode constructor receiver preparation.
//!
//! # Contents
//! - One prototype lookup and the shared canonical receiver allocator.
//! - Source-proven initial shape, field capacity and prototype proofs.
//! - Seven-completion provisional and future finalized family roots.
//!
//! # Invariants
//! Every moving construct operand remains in SyncJsCallRoots. The allocator
//! publishes the exact layout ticket before receiver allocation; completion
//! reads canonical frame roots, never a profile's weak receiver. Provisional
//! roots are never baked into receiver plans. Final roots affect only
//! future cells; all allocated cells keep their original physical capacity.
//!
//! # See also
//! - crate::constructor_layout owns family state and terminal sampling.
//! - crate::interp::host_call publishes canonical frame receiver/ticket fields.

use super::*;
use crate::native_abi::CommittedValueError;
impl Interpreter {
    /// Allocate the receiver one bytecode constructor body will initialize:
    /// OrdinaryCreateFromConstructor plus this engine's constructor feedback.
    ///
    /// Every construct path — generated linkage, the runtime construct
    /// boundary, and the interpreter's own `New` — hands the constructor body
    /// the same receiver contract through this one function: source-proven
    /// fields have their slab capacity reserved ahead of the first store,
    /// and a simple constructor receives its final hidden
    /// class with undefined slots, which its straight-line writes overwrite
    /// in source order before anything can observe them. `roots.scratch_0`
    /// holds the resolved prototype and `roots.new_target` the construct's
    /// `new.target`; the returned value is also left in `roots.receiver`.
    ///
    /// The first seven receivers reserve 64 persistent words on an unbakeable
    /// family lineage. Future receivers use only the published final root;
    /// source-proven fields and canonical overflow growth use that same root.
    pub(crate) fn allocate_bytecode_constructor_receiver(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        roots: &SyncJsCallRoots,
    ) -> Result<Value, VmError> {
        // The receiver is created on its prototype's lineage
        // (OrdinaryCreateFromConstructor); family preparation is rooted and may collect.
        let layout = self.constructor_layout_for_receiver(
            function_id,
            roots.new_target.get(),
            roots.scratch_0.get(),
            |vm, target| vm.constructor_field_bound(context, function_id, target),
        )?;
        let root = if layout.is_null() {
            // Legacy host objects with internal [[Construct]] have no current
            // constructor owner sidecar. Canonical allocation remains correct;
            // no finalized generated receiver plan is published for them.
            let bound = self.constructor_field_bound(context, function_id, roots.new_target.get());
            self.value_root(
                roots.scratch_0.get(),
                crate::object::receiver_inline_capacity(bound),
                crate::object::ShapeState::ORDINARY,
            )?
        } else {
            self.gc_heap.read_payload(
                layout,
                crate::constructor_layout::ConstructorLayoutBody::root,
            )
        };
        roots.set_construct_layout(layout);
        let reserved_field_count = self.prepare_constructor_field_transitions(
            context,
            function_id,
            roots.new_target.get(),
            roots.scratch_0.get(),
            root,
            layout,
            roots,
        )?;
        let simple_shape = self.jit_simple_constructor_shape(
            context,
            function_id,
            roots.scratch_0.get(),
            root,
            roots,
        )?;
        let field_count = simple_shape
            .map_or(0, |(_, fields)| fields)
            .max(reserved_field_count);
        let receiver = self.alloc_runtime_rooted_object_with_shape(root, &[], &[])?;
        roots.receiver.set(Value::object(receiver));
        self.publish_constructor_receiver_ticket(context, function_id, roots)?;
        if let Some((shape, initial_fields)) = simple_shape {
            let receiver = roots
                .receiver
                .get()
                .as_object()
                .ok_or(VmError::InvalidOperand)?;
            let mut slots = SmallVec::<[Value; 8]>::new();
            slots.resize(initial_fields, Value::undefined());
            crate::object::install_fresh_shape_with_slots(
                receiver,
                &mut self.gc_heap,
                shape,
                slots.as_slice(),
                field_count,
            );
        } else if field_count > crate::object::shape_body::inline_capacity_of(root) {
            // Keep the persistent inline prefix and reserve only its suffix.
            let mut receiver = roots
                .receiver
                .get()
                .as_object()
                .ok_or(VmError::InvalidOperand)?;
            crate::object::reserve_fresh_object_slot_capacity(
                &mut receiver,
                &mut self.gc_heap,
                reserved_field_count,
            )
            .map_err(VmError::from)?;
            roots.receiver.set(Value::object(receiver));
        }
        Ok(roots.receiver.get())
    }

    /// Install one freshly allocated receiver on its published canonical
    /// callee before SyncJsCallRoots ends. The original receiver is separate
    /// from mutable this and survives super's explicit object substitution.
    /// No allocation, reentry or collection occurs here.
    fn publish_constructor_receiver_ticket(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        roots: &SyncJsCallRoots,
    ) -> Result<(), VmError> {
        let frame = self.jit_innermost_native_frame();
        // SAFETY: the canonical receiver allocator is called only from the
        // entered constructor's production preparation entry; that frame is
        // initialized and published through the complete operation.
        let frame = unsafe { frame.as_mut() }.ok_or(VmError::InvalidOperand)?;
        if frame.header.function_id != function_id
            || !frame.is_construct()
            || frame.new_target_value != roots.new_target.get()
            || !frame.construct_layout.is_null()
        {
            return Err(VmError::InvalidOperand);
        }
        frame.construct_layout = roots.construct_layout();
        frame.construct_receiver = roots.receiver.get();
        self.note_published_constructor_receiver(Some(context), frame, roots.receiver.get())?;
        self.release_sampled_construct_ticket(frame);
        Ok(())
    }

    /// The sole post-publication source-feedback owner, shared by canonical
    /// allocation and the generated actual-family fit. No allocation/reentry
    /// occurs; malformed completed state is an internal failure, never a Miss.
    /// The optional admitted source is borrowed; caller ownership is resolved
    /// only when recording an actual source allocation requires it.
    pub(crate) fn note_published_constructor_receiver(
        &self,
        context: Option<&ExecutionContext>,
        frame: &crate::native_abi::Frame,
        receiver: Value,
    ) -> Result<(), VmError> {
        if !frame.is_construct()
            || frame.construct_receiver != receiver
            || receiver.as_object().is_none()
        {
            return Err(VmError::InvalidOperand);
        }
        if !frame.construct_layout.is_null()
            && self.gc_heap.read_payload(frame.construct_layout, |body| {
                body.base_function_id() != frame.header.function_id
            })
        {
            return Err(VmError::InvalidOperand);
        }
        // SELF/caller/ticket are already traced. Scalar feedback is written
        // only after complete receiver publication and retains no GC value.
        if let Some(caller) = unsafe { frame.caller_frame().as_ref() } {
            self.record_prepared_construct_family(context, caller, frame.construct_layout)?;
        }
        Ok(())
    }

    /// Upper bound of static own stores before choosing the receiver's layout root.
    /// Dynamic field slack is measured only at terminal construction completion.
    fn constructor_field_bound(
        &self,
        context: &ExecutionContext,
        base: u32,
        target: Value,
    ) -> usize {
        let mut functions = smallvec::SmallVec::<[u32; 4]>::new();
        functions.push(base);
        let mut current = target;
        let mut visited = smallvec::SmallVec::<[Value; 4]>::new();
        loop {
            if visited.contains(&current) {
                break;
            }
            visited.push(current);
            let (callable, next) = if let Some(class) = current.as_class_constructor() {
                (
                    class.ctor(&self.gc_heap),
                    Some(class.ctor_proto(&self.gc_heap)),
                )
            } else {
                (current, None)
            };
            if let Some(fid) = callable.as_function().or_else(|| {
                callable
                    .as_closure(&self.gc_heap)
                    .map(|closure| closure.function_id())
            }) && !functions.contains(&fid)
            {
                functions.push(fid);
            }
            let Some(next) = next else {
                break;
            };
            if !next.as_class_constructor().is_some() {
                break;
            }
            current = next;
        }
        functions
            .into_iter()
            .filter_map(|fid| {
                let owner = context.for_function(fid).ok()?;
                let function = owner.exec_function(fid)?;
                Some(
                    crate::constructor_fast_path::match_constructor_shape_stores(&owner, function)
                        .len(),
                )
            })
            .sum()
    }

    /// Resolve the hidden class a conservative generated constructor can own
    /// before its body begins.
    ///
    /// The matcher admits only straight-line own data writes to `this` followed
    /// by `return undefined`. Every matching name must also be absent from the
    /// selected prototype chain. Installing the final shape with undefined
    /// slots is therefore unobservable: the constructor cannot expose or
    /// inspect its receiver before overwriting those slots in source order.
    fn jit_simple_constructor_shape(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        prototype: Value,
        root: crate::object::ShapeHandle,
        roots: &SyncJsCallRoots,
    ) -> Result<Option<(crate::object::ShapeHandle, usize)>, VmError> {
        let Ok(owner) = context.for_function(function_id) else {
            return Ok(None);
        };
        let context = &*owner;
        let Some(function) = context.exec_function(function_id) else {
            return Ok(None);
        };
        let Some(init) = self.simple_constructor_init(context, function_id, function) else {
            return Ok(None);
        };
        let Some(proto_obj) = prototype.as_object() else {
            return Ok(None);
        };
        // The field names stay absent from the chain while the chain's
        // current proof is the one they were found absent under.
        let proof = crate::object::prototype_validity::chain_validity(proto_obj, &self.gc_heap);
        let proven = proof.as_ref().is_some_and(|proof| {
            self.simple_constructor_absence
                .get(&function_id)
                .is_some_and(|known| std::sync::Arc::ptr_eq(known, proof) && known.is_valid())
        });
        if !proven {
            if init.fields.iter().any(|field| {
                !matches!(
                    crate::object::lookup(proto_obj, &self.gc_heap, &field.name),
                    crate::object::PropertyLookup::Absent
                )
            }) {
                self.simple_constructor_absence.remove(&function_id);
                return Ok(None);
            }
            if let Some(proof) = proof {
                self.simple_constructor_absence.insert(function_id, proof);
            }
        }
        let field_count = init.fields.len();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            otter_gc::ExtraRootSource::visit_extra_roots(roots, visitor);
        };
        let shape = self.simple_constructor_shape_with_roots(
            function_id,
            root,
            &init,
            &mut external_visit,
        )?;
        Ok(Some((shape, field_count)))
    }

    fn simple_constructor_init(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        function: &CodeBlock,
    ) -> Option<crate::constructor_fast_path::SimpleConstructorInit> {
        if let Some(cached) = self.simple_constructor_init_cache.get(&function_id) {
            return cached.clone();
        }
        let init = crate::constructor_fast_path::match_simple_constructor_init(context, function);
        self.simple_constructor_init_cache
            .insert(function_id, init.clone());
        init
    }

    /// The final hidden class of a simple constructor's receivers on
    /// `root`'s lineage, cached per constructor and prototype root.
    fn simple_constructor_shape_with_roots(
        &mut self,
        function_id: u32,
        root: crate::object::ShapeHandle,
        init: &crate::constructor_fast_path::SimpleConstructorInit,
        external_visit: &mut dyn FnMut(&mut dyn FnMut(*mut RawGc)),
    ) -> Result<crate::object::ShapeHandle, VmError> {
        let root_id = self.shape_runtime.id_for_handle(&self.gc_heap, root);
        if let Some(shape) = self
            .simple_constructor_shape_cache
            .get(&(function_id, root_id))
        {
            return Ok(*shape);
        }

        let mut shape = root;
        for field in &init.fields {
            if let Some(child) = self.shape_runtime.child_if_cached(
                &self.gc_heap,
                shape,
                &field.name,
                crate::object::PropertyFlags::data_default(),
                false,
            ) {
                shape = child;
                continue;
            }
            shape = self
                .shape_runtime
                .child_with_roots(
                    &mut self.gc_heap,
                    shape,
                    &field.name,
                    crate::object::PropertyFlags::data_default(),
                    false,
                    external_visit,
                )
                .map_err(VmError::from)?;
        }
        if !crate::object::shape_body::state_of(root).is_provisional() {
            self.simple_constructor_shape_cache
                .insert((function_id, root_id), shape);
        }
        Ok(shape)
    }

    /// Prepare the receiver for one compiler-generated base constructor.
    ///
    /// The dynamic callable has already passed the generated identity guard.
    /// This owns the observable `new.target.prototype` lookup and
    /// `OrdinaryCreateFromConstructor` allocation, but does not start the
    /// constructor body. The caller's call-site root homes keep its arguments
    /// live while this local root provider protects the callable, prototype,
    /// and freshly allocated receiver across reentrant accessors and moving GC.
    pub fn jit_prepare_base_construct_receiver(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function_id: u32,
        callee: Value,
        new_target: Value,
    ) -> Result<Value, CommittedValueError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        self.jit_runtime_stats.runtime_constructs =
            self.jit_runtime_stats.runtime_constructs.saturating_add(1);
        let roots = SyncJsCallRoots::construct(callee, new_target, SmallVec::new());
        let _roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let new_target = roots.new_target.get();
        let proto = self
            .construct_prototype_for_callee(stack, context, &new_target)?
            .unwrap_or(
                self.constructor_prototype_value("Object")
                    .map_err(|error| CommittedValueError::Fatal(error.into()))?,
            );
        roots.scratch_0.set(proto);
        self.allocate_bytecode_constructor_receiver(context, function_id, &roots)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))
    }

    /// Prepare the source-proven base-to-derived class shape chain without
    /// an observable `prototype` lookup. Materialized construct callers may
    /// prepare eligible finalized receiver metadata before canonical allocation;
    /// this observation itself never reserves an out-of-line receiver slab.
    /// [`Self::observe_class_constructor_field_transitions`] may allocate
    /// shapes, so the caller's callee, `new.target` and arguments ride the
    /// traced anchor stack and are written back relocated. Actual stores keep
    /// their source timing through executable property feedback or canonical
    /// completion.
    pub(crate) fn observe_class_constructor_field_transitions_rooted(
        &mut self,
        context: &ExecutionContext,
        callee: &mut Value,
        new_target: &mut Value,
        args: &mut [Value],
    ) -> Result<(), VmError> {
        let anchor = self.push_iteration_anchor(*callee) - 1;
        self.push_iteration_anchor(*new_target);
        for &arg in args.iter() {
            self.push_iteration_anchor(arg);
        }
        let observed = self.observe_class_constructor_field_transitions(context, *new_target);
        *callee = self.iteration_anchor(anchor);
        *new_target = self.iteration_anchor(anchor + 1);
        for (index, arg) in args.iter_mut().enumerate() {
            *arg = self.iteration_anchor(anchor + 2 + index);
        }
        self.pop_iteration_anchors_to(anchor);
        observed
    }

    pub(crate) fn observe_class_constructor_field_transitions(
        &mut self,
        context: &ExecutionContext,
        new_target: Value,
    ) -> Result<(), VmError> {
        let Some(class) = new_target.as_class_constructor() else {
            return Ok(());
        };
        let super_constructor = class.ctor_proto(&self.gc_heap);
        let class_callable = class.ctor(&self.gc_heap);
        let base_callable = if super_constructor.is_undefined() {
            class_callable
        } else {
            super_constructor
                .as_class_constructor()
                .map(|class| class.ctor(&self.gc_heap))
                .unwrap_or(super_constructor)
        };
        let Some(base_function_id) = base_callable.as_function().or_else(|| {
            base_callable
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        }) else {
            return Ok(());
        };
        let derived_callable = class_callable;
        let Some(derived_function_id) = derived_callable.as_function().or_else(|| {
            derived_callable
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        }) else {
            return Ok(());
        };
        let mut chain_functions = smallvec::SmallVec::<[u32; 2]>::new();
        chain_functions.push(base_function_id);
        if derived_function_id != base_function_id {
            chain_functions.push(derived_function_id);
        }
        let mut store_count = 0usize;
        for function_id in chain_functions {
            let Ok(owner) = context.for_function(function_id) else {
                continue;
            };
            let Some(function) = owner.exec_function(function_id) else {
                continue;
            };
            store_count +=
                crate::constructor_fast_path::match_constructor_shape_stores(&owner, function)
                    .len();
        }
        if store_count == 0 || store_count > crate::object::MAX_INLINE_CAPACITY {
            return Ok(());
        }

        let roots = SyncJsCallRoots::construct(base_callable, new_target, SmallVec::new());
        roots
            .scratch_0
            .set(Value::object(class.prototype(&self.gc_heap)));
        let _roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let layout = self.constructor_layout_for_receiver(
            base_function_id,
            roots.new_target.get(),
            roots.scratch_0.get(),
            |_, _| store_count,
        )?;
        if layout.is_null() {
            return Ok(());
        }
        let root = self.gc_heap.read_payload(
            layout,
            crate::constructor_layout::ConstructorLayoutBody::root,
        );
        if crate::object::shape_body::state_of(root).is_provisional() {
            return Ok(());
        }
        self.prepare_constructor_field_transitions(
            context,
            base_function_id,
            roots.new_target.get(),
            roots.scratch_0.get(),
            root,
            layout,
            &roots,
        )?;
        Ok(())
    }

    fn prepare_constructor_field_transitions(
        &mut self,
        context: &ExecutionContext,
        base_function_id: u32,
        new_target: Value,
        prototype: Value,
        root: crate::object::ShapeHandle,
        layout: crate::constructor_layout::ConstructorLayout,
        roots: &SyncJsCallRoots,
    ) -> Result<usize, VmError> {
        // Only a class wrapper or exact ordinary new.target admits this
        // source-proven shape preparation. It reserves field capacity and
        // records the prototype proof consumed by receiver allocation plans.
        // Executable property feedback owns generated StoreProperty programs;
        // receiver preparation never retires code for another closure's root.
        // Other new.targets keep canonical Reflect.construct preparation.
        let class_new_target = new_target.is_class_constructor();
        let callable_new_target = new_target
            .as_class_constructor()
            .map(|class| class.ctor(&self.gc_heap))
            .unwrap_or(new_target);
        let new_target_function_id = callable_new_target.as_function().or_else(|| {
            callable_new_target
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        });
        if !class_new_target && new_target_function_id != Some(base_function_id) {
            return Ok(0);
        }
        let derived_function_id = new_target_function_id.filter(|&function_id| {
            function_id != base_function_id
                && context
                    .for_function(function_id)
                    .ok()
                    .and_then(|owner| {
                        owner
                            .exec_function(function_id)
                            .map(|function| function.is_derived_constructor)
                    })
                    .unwrap_or(false)
        });
        if crate::object::shape_body::state_of(root).is_provisional() {
            return Ok(0);
        }
        let Some(mut prototype_object) = prototype.as_object() else {
            return Ok(0);
        };
        // First publication migrated and registered the entire selected
        // ordinary chain. Its owned proof cannot revive after any mutation.
        // An exact finalized root and collector-updated prototype therefore
        // reuse that result before another chain walk, shape registration or
        // watchpoint lock/Arc clone. A changed prototype/root or invalid proof
        // follows the same canonical preparation below.
        if !layout.is_null()
            && let Some(capacity) = self.gc_heap.read_payload(layout, |body| {
                body.prepared_capacity(root, roots.scratch_0.get())
            })
        {
            return Ok(capacity);
        }

        // Class prototypes are initially assembled in dictionary storage. A
        // generated guard cannot name that mutable identity, so converge the
        // selected prototype chain onto the ordinary hidden-class path before
        // recording it. The migration roots and refreshes `prototype_object`.
        self.migrate_slow_to_fast(&mut prototype_object);
        roots.scratch_0.set(Value::object(prototype_object));
        let Some(prototype_validity) =
            crate::object::prototype_validity::chain_validity(prototype_object, &self.gc_heap)
        else {
            return Ok(0);
        };

        let mut functions = smallvec::SmallVec::<[u32; 2]>::new();
        functions.push(base_function_id);
        if let Some(function_id) = derived_function_id {
            functions.push(function_id);
        }

        let mut shape = root;
        let mut slot = 0u16;
        let mut seen = rustc_hash::FxHashSet::default();
        for function_id in functions {
            let Ok(owner) = context.for_function(function_id) else {
                return Ok(usize::from(slot));
            };
            let Some(function) = owner.exec_function(function_id) else {
                return Ok(usize::from(slot));
            };
            let stores =
                crate::constructor_fast_path::match_constructor_shape_stores(&owner, function);
            let simple_init = (function_id == base_function_id)
                .then(|| {
                    crate::constructor_fast_path::match_simple_constructor_init(&owner, function)
                })
                .flatten();
            for store in stores {
                // Each transition below allocates a shape; the prototype is
                // read back from its rooted slot every step.
                let Some(prototype_object) = roots.scratch_0.get().as_object() else {
                    return Ok(usize::from(slot));
                };
                if !seen.insert(store.name.clone())
                    || !matches!(
                        crate::object::lookup(prototype_object, &self.gc_heap, &store.name),
                        crate::object::PropertyLookup::Absent
                    )
                {
                    return Ok(usize::from(slot));
                }
                let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    otter_gc::ExtraRootSource::visit_extra_roots(roots, visitor);
                };
                shape = if let Some(child) = self.shape_runtime.child_if_cached(
                    &self.gc_heap,
                    shape,
                    &store.name,
                    crate::object::PropertyFlags::data_default(),
                    false,
                ) {
                    child
                } else {
                    self.shape_runtime
                        .child_with_roots(
                            &mut self.gc_heap,
                            shape,
                            &store.name,
                            crate::object::PropertyFlags::data_default(),
                            false,
                            &mut external_visit,
                        )
                        .map_err(VmError::from)?
                };
                slot = slot.checked_add(1).ok_or(VmError::InvalidOperand)?;
            }
            if let Some(init) = simple_init {
                self.simple_constructor_init_cache
                    .insert(function_id, Some(init));
                let root_id = self.shape_runtime.id_for_handle(&self.gc_heap, root);
                self.simple_constructor_shape_cache
                    .insert((function_id, root_id), shape);
            }
        }
        // Preparation belongs to the actual family, rather than a root-id
        // side map. Its GC allocation accounts these scalar/Arc payload bytes
        // and releases them with the family. No additional moving root is
        // cached, and a partially prepared prefix above is never published.
        let capacity = usize::from(slot);
        // The void migrator can stop after an allocation refusal. A chain
        // proof may still cover dictionary links, so it alone cannot prove
        // whole-chain migration finished. Such a partial result stays uncached
        // and retries canonical migration on the next receiver preparation.
        if !layout.is_null()
            && crate::constructor_layout::ConstructorLayoutBody::preparation_chain_is_ordinary(
                roots.scratch_0.get(),
                &self.gc_heap,
            )
        {
            self.gc_heap.with_payload(layout, |body| {
                body.publish_preparation(root, capacity, prototype_validity);
            });
        }
        Ok(capacity)
    }
}
