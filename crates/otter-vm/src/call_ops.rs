//! Call and construct opcode helpers.
//!
//! Stack-modifying call bytecodes decode variadic executable operands, prepare
//! frames, and may immediately invoke native/proxy/constructor paths. Keeping
//! that machinery here lets `lib.rs` stay closer to a dispatch map.
//!
//! # Contents
//! - Ordinary call entry and shared callable invocation.
//! - Constructor call entry, receiver/prototype setup and learned slot capacity.
//! - Spread and explicit-`this` call forms.
//! - Same-stack synchronous re-entry and reusable lean callback frames.
//! - Dispatch-local owner resolution for cross-chunk callees.
//!
//! # Invariants
//! - Call-site helpers advance the caller PC before pushing or synchronously
//!   invoking another frame.
//! - `invoke` remains the shared call path for bytecode, closures, native
//!   callables, bound functions, class constructors, and proxies.
//! - Constructor dispatch preserves `new.target` and receiver substitution
//!   invariants used by `pop_frame`. Base dispatch roots the prototype lookup
//!   it owns, records whether the receiver used generic `%Object.prototype%`
//!   fallback, and passes that rooted receiver plus provenance through the
//!   native boundary. Constructor arguments remain in their canonical root
//!   slots until that observable lookup and receiver allocation finish.
//! - Derived bytecode constructors enter with no receiver and preserve the
//!   caller's stable argument window; their direct `super(...)` dispatch owns
//!   the single prototype lookup and receiver allocation.
//! - Receiver observations are untraced and consumed before GC movement;
//!   no constructor profile retains an instance graph across collection.
//! - Constructor samples contain only scalar data; the constructing closure
//!   is resolved from a rewritten root after receiver allocation.
//! - Forwarded-call operands are reloaded after arguments materialization;
//!   the committed method lookup is never replayed.
//! - Generated receiver allocation uses the same VM-planned shape/capacity
//!   contract as the ordinary allocator; a nursery-window miss returns here
//!   while all constructor inputs remain rooted and no effect has started.
//! - Nested call/construct dispatch appends above an `ActivationFloor` on the
//!   current rooted stack; native boundary slots are collector-rewritten in
//!   their original storage.
//! - Cross-chunk call resolution caches owned contexts only for one dispatch
//!   and invalidates them against the code-space publication epoch.
//! - A freshly-started generator remains in a moving GC root through observable
//!   `prototype` lookup and publication into the caller.
//! - Every bytecode frame is built with its exact SELF; building a frame
//!   allocates no GC memory, so call-site locals stay current until the frame
//!   is published. The callee reaches its outer bindings through SELF's
//!   context and creates its own contexts in its prologue.
//!
//! # See also
//! - [`crate::Frame`]
//! - [`crate::executable`]

use std::cell::UnsafeCell;

use crate::constructor_profile::ConstructorProfileSample;

use crate::activation_stack::ActivationStack;
use otter_gc::raw::RawGc;
use smallvec::SmallVec;

use crate::{
    CodeBlock, ExecutionContext, Interpreter, JsObject, NativeCallInfo, NativeCtx, NativeFunction,
    Value, VmError, VmGetOutcome, VmPropertyKey,
    argument_window::{ArgumentOperands, BytecodeArgumentWindow},
    executable::OperandView,
    native_to_vm_error_with_stack,
    operand_decode::register_operand,
    read_register,
    runtime_cx::NativeCallRoots,
};

/// Mutable root state for synchronous JS re-entry before a callee frame owns
/// the values. Bound/proxy unwrapping replaces these fields in place; the
/// registered provider therefore rewrites the exact cells the dispatch loop
/// reads after any moving collection, rather than merely keeping duplicate
/// handle-arena entries alive.
struct JsCallRootSlot(UnsafeCell<Value>);

impl JsCallRootSlot {
    fn new(value: Value) -> Self {
        Self(UnsafeCell::new(value))
    }

    #[inline]
    fn get(&self) -> Value {
        // SAFETY: VM re-entry and GC run on one mutator thread. This short read
        // never spans a VM call or safepoint.
        unsafe { *self.0.get() }
    }

    #[inline]
    fn set(&self, value: Value) {
        // SAFETY: same single-mutator contract as `get`; no reference into the
        // slot escapes this non-allocating store.
        unsafe { *self.0.get() = value };
    }

    fn trace(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: the collector is the only writer while this callback runs;
        // all ordinary state operations are short and cannot trigger GC.
        unsafe { (&mut *self.0.get()).trace_value_slot_mut(visitor) };
    }
}

pub(crate) struct SyncJsCallRoots {
    current: JsCallRootSlot,
    receiver: JsCallRootSlot,
    new_target: JsCallRootSlot,
    proxy_target: JsCallRootSlot,
    args: UnsafeCell<SmallVec<[Value; 8]>>,
    scratch_0: JsCallRootSlot,
    scratch_1: JsCallRootSlot,
}

impl SyncJsCallRoots {
    pub(crate) fn call(current: Value, receiver: Value, args: SmallVec<[Value; 8]>) -> Self {
        Self {
            current: JsCallRootSlot::new(current),
            receiver: JsCallRootSlot::new(receiver),
            new_target: JsCallRootSlot::new(Value::undefined()),
            proxy_target: JsCallRootSlot::new(Value::undefined()),
            args: UnsafeCell::new(args),
            scratch_0: JsCallRootSlot::new(Value::undefined()),
            scratch_1: JsCallRootSlot::new(Value::undefined()),
        }
    }

    fn construct(current: Value, new_target: Value, args: SmallVec<[Value; 8]>) -> Self {
        Self {
            current: JsCallRootSlot::new(current),
            receiver: JsCallRootSlot::new(Value::undefined()),
            new_target: JsCallRootSlot::new(new_target),
            proxy_target: JsCallRootSlot::new(Value::undefined()),
            args: UnsafeCell::new(args),
            scratch_0: JsCallRootSlot::new(Value::undefined()),
            scratch_1: JsCallRootSlot::new(Value::undefined()),
        }
    }

    #[inline]
    pub(crate) fn target(&self) -> Value {
        self.current.get()
    }

    pub(crate) fn receiver_value(&self) -> Value {
        self.receiver.get()
    }

    pub(crate) fn set_receiver(&self, value: Value) {
        self.receiver.set(value);
    }

    pub(crate) fn scratch(&self, index: usize) -> Value {
        match index {
            0 => self.scratch_0.get(),
            1 => self.scratch_1.get(),
            _ => unreachable!("synchronous call roots expose two scratch slots"),
        }
    }

    pub(crate) fn set_scratch(&self, index: usize, value: Value) {
        match index {
            0 => self.scratch_0.set(value),
            1 => self.scratch_1.set(value),
            _ => unreachable!("synchronous call roots expose two scratch slots"),
        }
    }

    pub(crate) fn args_len(&self) -> usize {
        // SAFETY: this short read cannot allocate or overlap root tracing.
        unsafe { (&*self.args.get()).len() }
    }

    pub(crate) fn replace_args(&self, args: SmallVec<[Value; 8]>) {
        // SAFETY: short single-mutator write with no VM allocation.
        unsafe { *self.args.get() = args };
    }

    pub(crate) fn take_args(&self) -> SmallVec<[Value; 8]> {
        // SAFETY: moving the SmallVec itself cannot run GC. The caller must
        // transfer it into traced frame storage or install a slice provider
        // before the next possible collection.
        unsafe { std::mem::take(&mut *self.args.get()) }
    }
}

impl otter_gc::ExtraRootSource for SyncJsCallRoots {
    fn visit_extra_roots(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        self.current.trace(visitor);
        self.receiver.trace(visitor);
        self.new_target.trace(visitor);
        self.proxy_target.trace(visitor);
        self.scratch_0.trace(visitor);
        self.scratch_1.trace(visitor);
        // SAFETY: root tracing is the only operation active on this state while
        // GC runs; ordinary reads/writes never hold a borrow across safepoints.
        let args = self.args.get();
        let (ptr, len) = unsafe { ((*args).as_mut_ptr(), (*args).len()) };
        for index in 0..len {
            unsafe { (&mut *ptr.add(index)).trace_value_slot_mut(visitor) };
        }
    }
}

pub(crate) fn invoke_native_call_with_roots(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &ExecutionContext,
    call: crate::native_function::NativeCallTarget,
    realm_global: Option<JsObject>,
    this_value: Value,
    value_roots: &[&Value],
    args: &[Value],
) -> Result<Value, VmError> {
    let call_info = NativeCallInfo::call(this_value);
    let slice_roots = [args];
    let roots = NativeCallRoots::new(&call_info, value_roots, &slice_roots);
    // Pushed (not installed) so any outer scope's value/slice roots
    // stay visible to scavenges triggered inside this native.
    let _roots_guard = interp
        .gc_heap
        .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
    debug_assert!(interp.gc_heap.has_frame_root_providers());
    if let Some(global) = realm_global {
        interp.with_host_realm_global(global, |interp| {
            let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(interp, stack);
            let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, Some(context));
            let raw = call.invoke(&mut ctx, args);
            raw.map_err(|e| native_to_vm_error_with_stack(interp, stack, e))
        })
    } else {
        let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(interp, stack);
        let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, Some(context));
        let raw = call.invoke(&mut ctx, args);
        raw.map_err(|e| native_to_vm_error_with_stack(interp, stack, e))
    }
}

impl Interpreter {
    /// Attribute one generated derived-constructor completion boundary.
    pub fn record_jit_derived_construct_result_transition(&mut self) {
        self.jit_runtime_stats.derived_construct_result_transitions = self
            .jit_runtime_stats
            .derived_construct_result_transitions
            .saturating_add(1);
    }

    /// Attribute one generated derived-`this` binding boundary.
    pub fn record_jit_derived_this_bind_transition(&mut self) {
        self.jit_runtime_stats.derived_this_bind_transitions = self
            .jit_runtime_stats
            .derived_this_bind_transitions
            .saturating_add(1);
    }

    /// Attribute one exact-class superclass resolution boundary.
    pub fn record_jit_class_super_resolution_transition(&mut self) {
        self.jit_runtime_stats.class_super_resolution_transitions = self
            .jit_runtime_stats
            .class_super_resolution_transitions
            .saturating_add(1);
    }

    /// Try the non-observable half of generated base-constructor receiver
    /// preparation.
    ///
    /// A hit requires an already materialized own data `prototype` on an
    /// ordinary function/closure, or the intrinsic prototype held by a class
    /// constructor. Accessors, proxies, bound functions, missing lazy
    /// prototypes, and every other uncertain shape miss before effects so the
    /// caller can enter [`Self::jit_prepare_base_construct_receiver`].
    pub fn jit_try_prepare_base_construct_receiver(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        callee: Value,
        new_target: Value,
    ) -> Result<Option<Value>, VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Alloc);

        let prototype = if let Some(function_id) = new_target.as_function().or_else(|| {
            new_target
                .as_closure(&self.gc_heap)
                .map(|closure| closure.cached_function_id)
        }) {
            let owner = new_target.as_closure(&self.gc_heap);
            match self.function_prototype_slot(context, owner, function_id) {
                Some((value, _)) if !value.is_hole() => value,
                _ => return Ok(None),
            }
        } else if let Some(class) = new_target.as_class_constructor() {
            Value::object(class.prototype(&self.gc_heap))
        } else {
            return Ok(None);
        };

        self.jit_runtime_stats.runtime_constructs =
            self.jit_runtime_stats.runtime_constructs.saturating_add(1);
        let roots = SyncJsCallRoots::construct(callee, new_target, SmallVec::new());
        let _roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        roots.scratch_0.set(if prototype.is_object_type() {
            prototype
        } else {
            self.constructor_prototype_value("Object")?
        });
        Ok(Some(self.allocate_bytecode_constructor_receiver(
            context,
            function_id,
            &roots,
        )?))
    }

    /// Allocate the receiver one bytecode constructor body will initialize:
    /// OrdinaryCreateFromConstructor plus this engine's constructor feedback.
    ///
    /// Every construct path — generated linkage, the runtime construct
    /// boundary, and the interpreter's own `New` — hands the constructor body
    /// the same receiver contract through this one function: the body's
    /// baked field-transition program has its slab capacity reserved ahead
    /// of the first store, and a simple constructor receives its final hidden
    /// class with undefined slots, which its straight-line writes overwrite
    /// in source order before anything can observe them. `roots.scratch_0`
    /// holds the resolved prototype and `roots.new_target` the construct's
    /// `new.target`; the returned value is also left in `roots.receiver`.
    ///
    /// Storage is reserved for the larger of the baked transition program and
    /// the instance size learned from the previous receiver prepared for the
    /// same constructor pair, so fields the body adds through a callee still
    /// land in pre-reserved slots instead of growing the slab store by store.
    fn allocate_bytecode_constructor_receiver(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        roots: &SyncJsCallRoots,
    ) -> Result<Value, VmError> {
        // The receiver is created on its prototype's lineage
        // (OrdinaryCreateFromConstructor); finding the root never collects.
        let root = self.value_root(roots.scratch_0.get())?;
        let (reserved_field_count, profile) =
            self.constructor_receiver_reservation(context, function_id, roots, root)?;
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
        let receiver = self.alloc_runtime_rooted_object_with_capacity(
            root,
            crate::object::receiver_inline_capacity(field_count),
            &[],
            &[],
        )?;
        roots.receiver.set(Value::object(receiver));
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
        } else if field_count > crate::object::MAX_INLINE_CAPACITY {
            // Fields that fit the in-object slots need no slab; reserving one
            // would move every slot out of line for nothing.
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
        let receiver = roots
            .receiver
            .get()
            .as_object()
            .ok_or(VmError::InvalidOperand)?;
        self.note_constructor_receiver(profile, roots.new_target.get(), receiver);
        Ok(roots.receiver.get())
    }

    /// Slot capacity a constructor's receiver must start with: the baked
    /// transition program folded with the instance size learned from the
    /// previous receiver prepared by the same constructor. Returns the
    /// profile the caller records the allocated receiver into.
    /// `roots.new_target` and `roots.scratch_0` (the prototype) must be set.
    fn constructor_receiver_reservation(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        roots: &SyncJsCallRoots,
        root: crate::object::ShapeHandle,
    ) -> Result<(usize, ConstructorProfileSample), VmError> {
        let baked_field_count = self.prepare_constructor_field_transitions(
            context,
            function_id,
            roots.new_target.get(),
            roots.scratch_0.get(),
            root,
            roots,
        )?;
        let sample = self.sample_constructor_profile(function_id, roots.new_target.get());
        Ok((baked_field_count.max(sample.learned), sample))
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
        if init.fields.iter().any(|field| {
            !matches!(
                crate::object::lookup(proto_obj, &self.gc_heap, &field.name),
                crate::object::PropertyLookup::Absent
            )
        }) {
            return Ok(None);
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
        self.simple_constructor_shape_cache
            .insert((function_id, root_id), shape);
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
    ) -> Result<Value, VmError> {
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
            .unwrap_or(self.constructor_prototype_value("Object")?);
        roots.scratch_0.set(proto);
        self.allocate_bytecode_constructor_receiver(context, function_id, &roots)
    }

    /// Build guarded, pre-reserved field-transition programs for one exact
    /// base→derived construction chain.
    ///
    /// Shape publication remains at each original `StoreProperty`. This phase
    /// only interns the child shapes and reserves hidden slab capacity while
    /// the selected prototype and `new.target` are rooted. A prototype field,
    /// proxy/value prototype, duplicate name, or excessive chain stops the
    /// plan before generated code can perform an effect.
    /// Learn the exact base-to-derived field chain from a class wrapper without
    /// performing an observable `prototype` lookup. This lets a legacy
    /// materialized `New` caller seed the replacement backend before the
    /// canonical construct runs; only receivers that fit the in-body slab are
    /// admitted because this observation does not reserve an out-of-line slab.
    /// [`Self::observe_class_constructor_field_transitions`] for a construct
    /// whose operands are raw locals: observing allocates shapes, so the
    /// callee, `new.target` and arguments ride the traced anchor stack across
    /// it and are written back relocated.
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
        let root = self.value_root(roots.scratch_0.get())?;
        self.prepare_constructor_field_transitions(
            context,
            base_function_id,
            roots.new_target.get(),
            roots.scratch_0.get(),
            root,
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
        roots: &SyncJsCallRoots,
    ) -> Result<usize, VmError> {
        // Class wrappers and an exact ordinary `new.target` both provide the
        // stable constructor identity generated linkage needs. The latter is
        // important for non-simple function constructors: their first
        // `this.x = value` is an add-property transition, not an existing-slot
        // StoreProperty IC, so leaving it out would compile a body that exits
        // at that store on every generated entry. Distinct ordinary
        // `new.target` calls keep the canonical Reflect.construct preparation.
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
        let chain_key = (
            base_function_id,
            derived_function_id.unwrap_or(base_function_id),
        );
        let root_id = self.shape_runtime.id_for_handle(&self.gc_heap, root);
        let proof_key = (chain_key.0, chain_key.1, root_id);
        if let Some(&capacity) = self.constructor_field_capacity_cache.get(&chain_key)
            && self
                .constructor_prototype_validity_cache
                .get(&proof_key)
                .is_some_and(|validity| validity.is_valid())
        {
            return Ok(capacity);
        }
        let Some(mut prototype_object) = prototype.as_object() else {
            return Ok(0);
        };
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
        let mut reopt_functions = smallvec::SmallVec::<[u32; 2]>::new();
        for function_id in functions {
            let Ok(owner) = context.for_function(function_id) else {
                break;
            };
            let Some(function) = owner.exec_function(function_id) else {
                break;
            };
            let stores =
                crate::constructor_fast_path::match_constructor_shape_stores(&owner, function);
            let simple_init = (function_id == base_function_id)
                .then(|| {
                    crate::constructor_fast_path::match_simple_constructor_init(&owner, function)
                })
                .flatten();
            let receiver_is_pre_shaped = simple_init.is_some();
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
                let from_shape = shape;
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
                let transition = crate::jit::JitConstructorFieldTransitionPlan {
                    from_shape: self.shape_runtime.id_for_handle(&self.gc_heap, from_shape),
                    to_shape: self.shape_runtime.id_for_handle(&self.gc_heap, shape),
                    prototype_validity: prototype_validity.clone(),
                    slot,
                };
                if !receiver_is_pre_shaped {
                    let transitions = self
                        .constructor_field_transition_cache
                        .entry(function_id)
                        .or_default();
                    let stale = transitions
                        .get(&store.byte_pc)
                        .is_none_or(|existing| !existing.prototype_validity.is_valid());
                    if stale {
                        transitions.insert(store.byte_pc, transition);
                        self.jit_runtime_stats.constructor_field_transition_installs = self
                            .jit_runtime_stats
                            .constructor_field_transition_installs
                            .saturating_add(1);
                        if !reopt_functions.contains(&function_id) {
                            reopt_functions.push(function_id);
                        }
                    }
                }
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
        // The first generated receiver preparation can discover these plans
        // after an earlier hot loop already compiled the constructor. Retire
        // that stale generation once; permanent function cells and generation
        // leases keep the already-selected current call safe, while the next
        // entry recompiles against the richer transition snapshot.
        let capacity = usize::from(slot);
        self.constructor_field_capacity_cache
            .entry(chain_key)
            .and_modify(|reserved| *reserved = (*reserved).max(capacity))
            .or_insert(capacity);
        self.constructor_prototype_validity_cache
            .insert(proof_key, prototype_validity);
        for function_id in reopt_functions {
            self.evict_compiled_for_reopt(function_id);
        }
        Ok(capacity)
    }

    /// Apply the derived-constructor return rules to one generated callee.
    pub fn jit_derived_construct_result(
        &mut self,
        result: Value,
        bound_this: Value,
    ) -> Result<Value, VmError> {
        if result.is_object_type() {
            Ok(result)
        } else if result.is_undefined() {
            if bound_this.is_hole() {
                Err(self.err_this_uninit(
                    "must call super constructor in derived class before accessing 'this' or returning from derived constructor"
                        .to_string()
                        .into(),
                ))
            } else {
                Ok(bound_this)
            }
        } else {
            Err(self.err_type(
                "derived constructors may only return an object or undefined"
                    .to_string()
                    .into(),
            ))
        }
    }

    /// Read the live superclass identity from an exact class-constructor
    /// wrapper. Non-class inputs return the hole sentinel so generated code can
    /// side-exit before any construct effect.
    pub fn jit_class_super_constructor(&self, value: Value) -> Value {
        value
            .as_class_constructor()
            .map_or_else(Value::hole, |class| class.ctor_proto(&self.gc_heap))
    }

    /// Copy a compiler-collected spread argument array into an unpublished
    /// native callee frame without allocation or observable JavaScript work.
    ///
    /// # Safety
    ///
    /// `frame` must name an initialized, exclusively owned [`Frame`]
    /// whose register window remains live for this call. It is deliberately
    /// not published until the generated caller completes this copy.
    pub unsafe fn jit_copy_spread_arguments(
        &self,
        arguments: Value,
        frame: *mut crate::native_abi::Frame,
        parameter_count: u16,
    ) -> bool {
        let Some(array) = arguments.as_array() else {
            return false;
        };
        // SAFETY: upheld by the generated-linkage caller; the view validates
        // the raw frame and window descriptors before exposing scalar writes.
        let Ok(mut frame) = (unsafe { crate::ActiveFrameMut::from_ptr(frame) }) else {
            return false;
        };
        if usize::from(parameter_count) > frame.register_count() {
            return false;
        }
        crate::array::with_elements(array, &self.gc_heap, |elements| {
            for (index, value) in elements
                .iter()
                .copied()
                .take(usize::from(parameter_count))
                .enumerate()
            {
                if frame.write(index as u16, value).is_err() {
                    return false;
                }
            }
            true
        })
    }

    pub(crate) fn bind_bytecode_call_arguments(
        &mut self,
        function: &CodeBlock,
        frame: &mut crate::PreparedCall,
        args: SmallVec<[Value; 8]>,
    ) -> Result<(), VmError> {
        if function.param_count > function.register_count {
            return Err(VmError::InvalidOperand);
        }
        frame.arguments = args;
        Ok(())
    }

    pub(crate) fn invoke_native_construct_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        native: NativeFunction,
        this_value: &Value,
        new_target: &Value,
        used_object_prototype_fallback: bool,
        args: &[Value],
    ) -> Result<Value, VmError> {
        // A cross-realm `new` runs under the constructor's realm, exactly
        // like the call path: intrinsics and default prototypes resolve
        // there.
        if let Some(global) = self.native_target_realm_global(&native)
            && global != self.global_this
        {
            return self.with_host_realm_global(global, |interp| {
                interp.invoke_native_construct_rooted(
                    stack,
                    context,
                    native,
                    this_value,
                    new_target,
                    used_object_prototype_fallback,
                    args,
                )
            });
        }
        let call = native.call_target(&self.gc_heap);
        let call_info = NativeCallInfo::construct_with_receiver(
            *this_value,
            Some(*new_target),
            used_object_prototype_fallback,
        );
        self.record_runtime_native_call()?;
        // Same root coverage as the call path (`invoke_native_call_with_roots`):
        // trace the interpreter's full root set (crucially the scope-handle
        // arena, so a native constructor's `Local` handles stay live) and
        // pin `this`, `new.target`, and the argument slice across every
        // scavenge the constructor triggers. Without this a native `new X(…)`
        // ran fully unrooted — e.g. `new Set([...])` stranded its iterable.
        let slice_roots = [args];
        let roots = NativeCallRoots::new(&call_info, &[], &slice_roots);
        let _roots_guard = self
            .gc_heap
            .register_extra_roots(otter_gc::ExtraRoots::new(&roots));
        let turn = crate::runtime_cx::RuntimeTurn::from_rooted_parts(self, stack);
        let mut ctx = NativeCtx::from_runtime_turn(turn, &call_info, Some(context));
        let raw = call.invoke(&mut ctx, args);
        let rooted_this = *ctx.this_value();
        let (interp, stack) = ctx.cx.into_parts();
        let result = raw.map_err(|e| native_to_vm_error_with_stack(interp, stack, e))?;
        // The constructor ran under its own realm. A body-slot exotic it
        // built carries no `[[Prototype]]`, so stamp the realm's
        // intrinsic before the value escapes into another one.
        interp.register_exotic_realm_proto(&result);
        Ok(if result.is_object_type() {
            result
        } else {
            rooted_this
        })
    }

    /// Bytecode function a staged request's callee names, for call feedback.
    /// A class wrapper names its constructor; every other kind names none.
    pub(crate) fn staged_bytecode_target(&self, stack: &mut ActivationStack) -> Option<u32> {
        let callee = stack.staged_request_mut()?.callee;
        let callee = callee
            .as_class_constructor()
            .map_or(callee, |class| class.ctor(&self.gc_heap));
        callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|closure| closure.function_id())
        })
    }

    /// Handle `Op::Call`: stage the callee with an `undefined` receiver for
    /// the trampoline, which classifies it and owns its activation.
    #[cfg(test)]
    pub(crate) fn do_call<'a>(
        &mut self,
        stack: &mut ActivationStack,
        _context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        self.do_call_inner(stack, ArgumentOperands::decoded(operands.into()))
    }

    pub(crate) fn do_call_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_call_inner(stack, ArgumentOperands::execution(function, instruction))
    }

    fn do_call_inner(
        &mut self,
        stack: &mut ActivationStack,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        // The call header (`dst`, callee, argc) reads from one operand-word
        // slice; a call with two or more arguments spills past the inline words,
        // so the header cannot use the fixed-arity accessors.
        let (dst, callee_reg, argc) = match operands.words() {
            Some([dst, callee, argc, ..]) => (*dst as u16, *callee as u16, *argc),
            Some(_) => return Err(VmError::InvalidOperand),
            None => (
                operands.register(0)?,
                operands.register(1)?,
                operands.const_index(2)?,
            ),
        };
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let args =
            BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc as usize)
                .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        stack.stage_call(callee, Value::undefined(), None, args, Some(dst));
        Ok(())
    }

    /// §15.10.3 PrepareForTailCall — `Op::TailCall`. The staged request
    /// replaces this activation at the trampoline, so a strict-mode tail call
    /// uses O(1) native stack. The compiler emits it only outside
    /// `try`/`finally`; a frame whose completion needs post-processing
    /// (constructors, suspension owners, live handlers) stages an ordinary
    /// call instead.
    pub(crate) fn do_tail_call_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let top_idx = stack.len().checked_sub(1).ok_or(VmError::InvalidOperand)?;
        let frame = &stack[top_idx];
        let tail_safe = !self.frame_has_suspension_owner(frame)
            && !frame.is_construct()
            && self
                .frame_cold(frame)
                .is_none_or(|cold| cold.handlers.is_empty());
        let return_destination = frame.return_destination;
        self.do_call_inner(stack, ArgumentOperands::execution(function, instruction))?;
        if tail_safe && let Some(request) = stack.staged_request_mut() {
            request.return_destination = return_destination;
            request.header.flags = crate::native_abi::NativeFrameFlags::from_bits(
                request.header.flags.bits() | crate::native_abi::NativeFrameFlags::TAIL_CALL,
            );
            self.frame_release_cold(&mut stack[top_idx]);
        }
        Ok(())
    }

    /// Stage `callee(...args)` with an explicit receiver; the completion is
    /// delivered to the caller's `dst` when its activation resumes.
    ///
    /// `caller_pc` must already be advanced so the resumed dispatch continues
    /// after the originating instruction.
    pub(crate) fn invoke(
        &mut self,
        stack: &mut ActivationStack,
        _context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) -> Result<(), VmError> {
        stack.stage_call(*callee, this_value, None, args, Some(dst));
        Ok(())
    }

    /// Stage `[[Construct]]` with an explicit `new.target`.
    pub(crate) fn stage_construct(
        &mut self,
        stack: &mut ActivationStack,
        callee: Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
        dst: u16,
    ) {
        stack.stage_call(
            callee,
            Value::undefined(),
            Some(new_target),
            args,
            Some(dst),
        );
    }

    /// Handle `Op::New`.
    #[cfg(test)]
    pub(crate) fn do_construct<'a>(
        &mut self,
        stack: &mut ActivationStack,
        _context: &ExecutionContext,
        operands: impl Into<OperandView<'a>>,
    ) -> Result<(), VmError> {
        self.do_construct_inner(stack, ArgumentOperands::decoded(operands.into()))
    }

    pub(crate) fn do_construct_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        self.do_construct_inner(stack, ArgumentOperands::execution(function, instruction))
    }

    fn do_construct_inner(
        &mut self,
        stack: &mut ActivationStack,
        operands: ArgumentOperands<'_>,
    ) -> Result<(), VmError> {
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let argc = operands.const_index(2)? as usize;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let args = BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc)
            .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        self.stage_construct(stack, callee, callee, args, dst);
        Ok(())
    }

    /// The `new.target` a `super(...)` call forwards: the derived frame's
    /// own, or the parent constructor itself outside a construct.
    fn super_new_target(frame: &crate::Frame, callee: Value) -> Value {
        let target = frame.new_target();
        if target.is_undefined() {
            callee
        } else {
            target
        }
    }

    /// Handle fixed-arity `Op::SuperConstruct`.
    pub(crate) fn do_super_construct_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let operands = ArgumentOperands::execution(function, instruction);
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let argc = operands.const_index(2)? as usize;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let new_target = Self::super_new_target(&stack[top_idx], callee);
        let args = BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 3, argc)
            .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        self.stage_construct(stack, callee, new_target, args, dst);
        Ok(())
    }

    fn spread_call_arguments(&self, value: Value) -> Result<SmallVec<[Value; 8]>, VmError> {
        let array = value.as_array().ok_or(VmError::TypeMismatch)?;
        Ok(crate::array::with_elements(
            array,
            &self.gc_heap,
            |elements| elements.iter().copied().collect(),
        ))
    }

    pub(crate) fn do_construct_spread(
        &mut self,
        stack: &mut ActivationStack,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let args = self.spread_call_arguments(*read_register(&stack[top_idx], args_reg)?)?;
        stack[top_idx].advance_pc()?;
        self.stage_construct(stack, callee, callee, args, dst);
        Ok(())
    }

    pub(crate) fn do_super_construct_spread(
        &mut self,
        stack: &mut ActivationStack,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let args_reg = register_operand(operands.get(2))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let new_target = Self::super_new_target(&stack[top_idx], callee);
        let args = self.spread_call_arguments(*read_register(&stack[top_idx], args_reg)?)?;
        stack[top_idx].advance_pc()?;
        self.stage_construct(stack, callee, new_target, args, dst);
        Ok(())
    }

    /// Handle `Op::CallSpread`: the receiver register holds the explicit
    /// `this` value and the arguments array holds every actual.
    pub(crate) fn do_call_spread(
        &mut self,
        stack: &mut ActivationStack,
        operands: OperandView<'_>,
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let callee_reg = register_operand(operands.get(1))?;
        let this_reg = register_operand(operands.get(2))?;
        let args_reg = register_operand(operands.get(3))?;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        let args = self.spread_call_arguments(*read_register(&stack[top_idx], args_reg)?)?;
        stack[top_idx].advance_pc()?;
        stack.stage_call(callee, this_value, None, args, Some(dst));
        Ok(())
    }

    /// Handle `Op::CallWithThis`: `Op::Call` with an explicit receiver
    /// register.
    pub(crate) fn do_call_with_this_exec(
        &mut self,
        stack: &mut ActivationStack,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let operands = ArgumentOperands::execution(function, instruction);
        let dst = operands.register(0)?;
        let callee_reg = operands.register(1)?;
        let this_reg = operands.register(2)?;
        let argc = operands.const_index(3)? as usize;
        let top_idx = stack.len() - 1;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        let args = BytecodeArgumentWindow::from_operands(&stack[top_idx], operands, 4, argc)
            .to_smallvec8()?;
        stack[top_idx].advance_pc()?;
        stack.stage_call(callee, this_value, None, args, Some(dst));
        Ok(())
    }

    /// Synchronously invoke `callee(args)` with the given `this` and return
    /// its completion, from a host callback with no enclosing turn.
    pub fn run_callable_sync(
        &mut self,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        let mut activations = ActivationStack::new();
        self.with_runtime_turn(&mut activations, |turn| {
            let (interp, stack) = turn.into_parts();
            interp.run_callable_sync_rooted(stack, context, callee, this_value, args)
        })
    }

    /// Synchronously invoke a callable above the current activation floor.
    ///
    /// The request enters the same classifying trampoline as a bytecode call;
    /// this host callback retains only its own Rust frame while it runs.
    pub(crate) fn run_callable_sync_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
        this_value: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        self.enter_sync_reentry()?;
        let floor = stack.floor();
        stack.stage_call(*callee, this_value, None, args, None);
        let result = self.execute_prepared_call(context, stack);
        self.release_frames_above(stack, floor);
        self.leave_sync_reentry();
        result
    }

    /// Synchronously construct above the current rooted activation floor.
    pub(crate) fn run_construct_sync_rooted(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: &Value,
        new_target: Value,
        args: SmallVec<[Value; 8]>,
    ) -> Result<Value, VmError> {
        if !stack.is_runtime_rooted_by(self) {
            return Err(VmError::InvalidOperand);
        }
        self.enter_sync_reentry()?;
        let floor = stack.floor();
        stack.stage_call(*target, Value::undefined(), Some(new_target), args, None);
        let result = self.execute_prepared_call(context, stack);
        self.release_frames_above(stack, floor);
        self.leave_sync_reentry();
        result
    }

    pub(crate) fn construct_prototype_for_callee(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        let function_id = callee.as_function().or_else(|| {
            callee
                .as_closure(&self.gc_heap)
                .map(|c| c.cached_function_id)
        });
        if let Some(function_id) = function_id {
            let owner = callee.as_closure(&self.gc_heap);
            return match self.function_property_get_with_receiver(
                stack,
                context,
                owner,
                function_id,
                Some(*callee),
                "prototype",
            )? {
                proto if proto.is_object_type() => Ok(Some(proto)),
                _ => Ok(None),
            };
        }
        if let Some(c) = callee.as_class_constructor() {
            return Ok(Some(Value::object(c.prototype(&self.gc_heap))));
        }
        if callee.as_proxy().is_some() {
            return self.construct_prototype_via_get(stack, context, callee);
        }
        if let Some(obj) = callee.as_object() {
            return Ok(match crate::object::get(obj, &self.gc_heap, "prototype") {
                Some(proto) if proto.is_object_type() => Some(proto),
                _ => None,
            });
        }
        if callee.is_bound_function() {
            return self.construct_prototype_via_get(stack, context, callee);
        }
        if let Some(native) = callee.as_native_function() {
            return native
                .own_property_descriptor(&mut self.gc_heap, "prototype")
                .map_err(|_| VmError::InvalidOperand)
                .map(|desc| {
                    desc.and_then(|d| match d.kind {
                        crate::object::DescriptorKind::Data { value } if value.is_object_type() => {
                            Some(value)
                        }
                        _ => None,
                    })
                });
        }
        Ok(None)
    }

    fn construct_prototype_via_get(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        callee: &Value,
    ) -> Result<Option<Value>, VmError> {
        let callee_anchor = self.push_iteration_anchor(*callee) - 1;
        let anchor_base = callee_anchor;
        let result = (|| -> Result<Option<Value>, VmError> {
            let key = VmPropertyKey::String("prototype");
            let callee = self.iteration_anchor(callee_anchor);
            let proto = match self.ordinary_get_value(stack, context, callee, callee, &key, 0)? {
                VmGetOutcome::Value(value) => value,
                VmGetOutcome::InvokeGetter { getter } => {
                    let callee = self.iteration_anchor(callee_anchor);
                    self.run_callable_sync_rooted(stack, context, &getter, callee, SmallVec::new())?
                }
            };
            // The getter may have collected or revoked a Proxy. Reload the
            // callee from the anchor before consulting its post-Get state.
            let revoked_proxy = self
                .iteration_anchor(callee_anchor)
                .as_proxy()
                .is_some_and(|proxy| proxy.is_revoked(&self.gc_heap));
            if !proto.is_object_type() && revoked_proxy {
                return Err(
                    self.err_type(("Cannot get prototype from a revoked proxy".to_string()).into())
                );
            }
            Ok(proto.is_object_type().then_some(proto))
        })();
        self.pop_iteration_anchors_to(anchor_base);
        result
    }

    /// Native constructors that must NOT receive a pre-allocated
    /// receiver whose prototype is read from `new.target` before the
    /// constructor body runs. `Promise` builds its own object, and the
    /// dynamic-function constructors (`Function` and friends) parse
    /// their source and only then run `GetPrototypeFromConstructor`
    /// (§20.2.1.1.1) — an eager `new.target.prototype` read here would
    /// be observable before the SyntaxError a bad body must throw. The
    /// buffer, view and typed-array constructors validate their arguments
    /// before `OrdinaryCreateFromConstructor` (§25.1.4.1, §25.3.2.1,
    /// §23.2.5.1) and allocate their own exotic result.
    pub(crate) fn native_receiverless_constructor(&self, callee: &Value) -> Option<NativeFunction> {
        let native = if let Some(native) = callee.as_native_function() {
            native
        } else {
            let obj = callee.as_object()?;
            crate::object::constructor_native(obj, &self.gc_heap)
                .and_then(|v| v.as_native_function())?
        };
        [
            "Promise",
            "Function",
            "GeneratorFunction",
            "AsyncFunction",
            "AsyncGeneratorFunction",
            // §22.2.4.1 RegExp and §26.1.* WeakRef /
            // FinalizationRegistry allocate their own exotic result and
            // resolve `new.target.prototype` themselves. Pre-allocating
            // an ordinary receiver here would read that property a
            // second time, and the getter is observable.
            "RegExp",
            "WeakRef",
            "FinalizationRegistry",
            "ArrayBuffer",
            "SharedArrayBuffer",
            "DataView",
            "Int8Array",
            "Uint8Array",
            "Uint8ClampedArray",
            "Int16Array",
            "Uint16Array",
            "Int32Array",
            "Uint32Array",
            "Float16Array",
            "Float32Array",
            "Float64Array",
            "BigInt64Array",
            "BigUint64Array",
        ]
        .iter()
        .any(|expected| native.name(&self.gc_heap).eq_str(expected, &self.gc_heap))
        .then_some(native)
    }

    /// Handle `Op::CallForwardArguments`: `callee.apply(this_arg, arguments)`
    /// for a body whose every use of `arguments` is such a forward.
    ///
    /// `method` already holds the observable `GetV(callee, "apply")`. When it
    /// is %Function.prototype.apply%, the activation's incoming arguments are
    /// forwarded to `callee` directly — the arguments object is never built.
    /// Any other method value receives the activation's arguments object,
    /// materialized once per frame, exactly as a materialized `arguments`
    /// binding would have supplied it.
    pub(crate) fn do_call_forward_arguments_exec(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        function: &CodeBlock,
        instruction: &crate::CodeBlockInstruction,
    ) -> Result<(), VmError> {
        let dst = function
            .register(instruction, 0)
            .ok_or(VmError::InvalidOperand)?;
        let method_reg = function
            .register(instruction, 1)
            .ok_or(VmError::InvalidOperand)?;
        let callee_reg = function
            .register(instruction, 2)
            .ok_or(VmError::InvalidOperand)?;
        let this_reg = function
            .register(instruction, 3)
            .ok_or(VmError::InvalidOperand)?;
        let top_idx = stack.len() - 1;
        let method = *read_register(&stack[top_idx], method_reg)?;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        if crate::method_ops::is_function_prototype_intrinsic_value(
            method,
            &self.gc_heap,
            crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply,
        ) {
            if !self.is_callable_runtime(&callee) {
                return Err(VmError::NotCallable);
            }
            let existing = stack[top_idx].arguments_object().map(Value::object);
            let forwarded = if let Some(arguments) = existing {
                self.create_list_from_array_like(stack, context, arguments)?
            } else {
                let view = crate::ActiveFrameRef::from_frame(&stack[top_idx]);
                let count = view.incoming_argument_count();
                let mut forwarded: SmallVec<[Value; 8]> = (0..count)
                    .map(|index| view.incoming_argument(index))
                    .collect::<Result<_, _>>()?;
                self.refresh_mapped_argument_values(
                    function,
                    &crate::ActiveFrameRef::from_frame(&stack[top_idx]),
                    &mut forwarded,
                )?;
                forwarded
            };
            let callee = *read_register(&stack[top_idx], callee_reg)?;
            let this_value = *read_register(&stack[top_idx], this_reg)?;
            stack[top_idx].advance_pc()?;
            return self.invoke(stack, context, &callee, this_value, forwarded, dst);
        }
        let arguments_object = self.materialize_frame_arguments_object(context, stack, top_idx)?;
        // Materialization can move a getter-produced method and both operands.
        // Their register slots retain the committed lookup without replaying it.
        let method = *read_register(&stack[top_idx], method_reg)?;
        let callee = *read_register(&stack[top_idx], callee_reg)?;
        let this_value = *read_register(&stack[top_idx], this_reg)?;
        stack[top_idx].advance_pc()?;
        let args: SmallVec<[Value; 8]> = [this_value, arguments_object].into_iter().collect();
        self.invoke(stack, context, &method, callee, args, dst)
    }
}
