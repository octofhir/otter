//! Interpreter kernels over binding contexts.
//!
//! Every captured or eval-visible binding lives in a context slot
//! ([`crate::context`]). These kernels implement the context opcodes for
//! interpreter dispatch and for the compiled tiers' committed runtime calls,
//! over the representation-neutral [`ActiveFrameMut`] or over already-read
//! boxed values.
//!
//! # Contents
//! - SELF reads: `LoadClosureContext`, `LoadSelf`.
//! - Allocation: `CreateContext`, `CopyContext`, and the module-init SELF
//!   closure.
//! - Slot access: `LoadContextSlot[Checked]`, `StoreContextSlot[Checked]`,
//!   `BindThisContextSlot`, with descriptor-named TDZ errors.
//! - Eval-extension lookups: `LoadLookupSlot`, `StoreLookupSlot`,
//!   `DeleteLookupSlot`, `Load/Typeof/Store/DeleteLookupGlobal`,
//!   `ResolveLookupRef` / `StoreRef`, `DeclareEvalVar`, `StoreVarScope`.
//! - [`Interpreter::eval_caller_chain`] — the descriptor chain a direct eval
//!   compiles against.
//!
//! # Invariants
//! - A context operand holds a context value or `undefined`; anything else is
//!   an invalid (unverified) operand.
//! - `Lookup*` probes the extensions of the contexts at hops `[0, depth)`
//!   only, innermost first; the declaring context's own extension is never
//!   probed for a slot target.
//! - Every allocating kernel re-reads each context it still needs from a
//!   traced location after the allocation: a pending body field, a traced
//!   register, or a local registered with the allocation's roots.
//! - Kernels never advance the PC.
//!
//! # See also
//! - [`crate::context`] — the context body and its heap helpers.
//! - [`crate::eval_env`] — eval extensions.
//! - [`crate::eval_ops`] — direct eval, which consumes the caller chain.

use crate::native_abi::CommittedValueError;
use otter_bytecode::{
    BindingStoreFallback, ContextCoord, EvalCallerChain, EvalCallerScope, LookupGlobalMode,
    LookupRefTarget, ScopeDescriptor, SlotKind, StoreRefMode, opcode_schema::BindingMissing,
};

use crate::activation_stack::ActivationStack;
use crate::context::{self, ContextHandle, ContextShape};
use crate::eval_env;
use crate::{ActiveFrameMut, ExecutionContext, Interpreter, Value, VmError};

/// `ReferenceError` message for reading a derived constructor's `this` before
/// `super()` bound it.
pub(crate) const DERIVED_THIS_UNINITIALIZED: &str = "must call super constructor in derived class before accessing 'this' or returning from derived constructor";

/// `ReferenceError` message for a second `super()` in one construction.
pub(crate) const SUPER_CALLED_TWICE: &str = "super constructor may only be called once";

/// Decode a context operand: a context, or `undefined` for "no context".
pub(crate) fn context_operand(value: Value) -> Result<Option<ContextHandle>, VmError> {
    if value.is_undefined() {
        return Ok(None);
    }
    value.as_context().map(Some).ok_or(VmError::InvalidOperand)
}

/// Decode a context operand that must name a context (a slot target exists).
fn required_context(value: Value) -> Result<ContextHandle, VmError> {
    value.as_context().ok_or(VmError::InvalidOperand)
}

fn decode_coord(coord: i32) -> Result<ContextCoord, VmError> {
    ContextCoord::from_imm32(coord).ok_or(VmError::InvalidOperand)
}

fn decode_depth(depth: i32) -> Result<u16, VmError> {
    u16::try_from(depth).map_err(|_| VmError::InvalidOperand)
}

/// Run `f` over the descriptor of `(function_id, scope_index)`, resolving the
/// owning chunk through `context`'s code space.
pub(crate) fn with_scope<R>(
    context: &ExecutionContext,
    function_id: u32,
    scope_index: u16,
    f: impl FnOnce(&ScopeDescriptor) -> R,
) -> Result<R, VmError> {
    let owner = context
        .for_function(function_id)
        .map_err(|_| VmError::InvalidOperand)?;
    let function = owner
        .exec_function(function_id)
        .ok_or(VmError::InvalidOperand)?;
    let scope = function
        .scopes
        .get(scope_index as usize)
        .ok_or(VmError::InvalidOperand)?;
    Ok(f(scope))
}

/// Exact DerivedThis slot from an immutable verified scope descriptor.
/// Multiple such slots are malformed metadata, never an ownership heuristic.
pub(crate) fn derived_this_slot(scope: &ScopeDescriptor) -> Result<Option<u16>, VmError> {
    let mut result = None;
    for (index, slot) in scope.slots.iter().enumerate() {
        if slot.kind == SlotKind::DerivedThis {
            if result.is_some() {
                return Err(VmError::InvalidOperand);
            }
            result = Some(u16::try_from(index).map_err(|_| VmError::InvalidOperand)?);
        }
    }
    Ok(result)
}

impl Interpreter {
    // ------------------------------------------------------------------
    // SELF
    // ------------------------------------------------------------------

    /// `LoadClosureContext dst`: SELF's closure context, or `undefined`.
    pub(crate) fn frame_load_closure_context(
        &self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = frame.closure_context(&self.gc_heap);
        frame.write(dst, value)
    }

    /// `LoadSelf dst`: the exact closure being executed.
    pub(crate) fn frame_load_self(
        &self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
    ) -> Result<(), VmError> {
        let value = frame.self_value();
        frame.write(dst, value)
    }

    // ------------------------------------------------------------------
    // Allocation
    // ------------------------------------------------------------------

    /// Allocate a context of `function_id`'s scope `scope_index` under
    /// `parent` (a context or `undefined`).
    pub(crate) fn create_context_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        scope_index: u32,
        parent: Value,
    ) -> Result<Value, VmError> {
        context_operand(parent)?;
        let scope_index = u16::try_from(scope_index).map_err(|_| VmError::InvalidOperand)?;
        let owner = context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let function = owner
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let descriptor = function
            .scopes
            .get(scope_index as usize)
            .ok_or(VmError::InvalidOperand)?;
        let shape = ContextShape {
            scope_function_id: function_id,
            scope_index,
            slot_count: u16::try_from(descriptor.slots.len())
                .map_err(|_| VmError::InvalidOperand)?,
            has_extension: descriptor.flags.has_extension,
        };
        // `parent` rides in the pending body, so a collection here rewrites it.
        let handle = context::alloc_context_with_roots(
            &mut self.gc_heap,
            shape,
            parent,
            |index| descriptor.slots[index].kind.initial_hole(),
            &mut |_| {},
        )
        .map_err(crate::oom_to_vm)?;
        Ok(Value::context(handle))
    }

    /// `CreateContext dst, parent, scope` over the running frame's own scope
    /// table.
    pub(crate) fn frame_create_context(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        parent_reg: u16,
        scope_index: u32,
    ) -> Result<(), VmError> {
        let parent = frame.read(parent_reg)?;
        let function_id = frame.function_id();
        let owns_this = with_scope(
            context,
            function_id,
            u16::try_from(scope_index).map_err(|_| VmError::InvalidOperand)?,
            derived_this_slot,
        )??
        .is_some();
        let created = self.create_context_value(context, function_id, scope_index, parent)?;
        frame.write(dst, created)?;
        if owns_this {
            frame.publish_derived_this_context(
                function_id,
                created.as_context().ok_or(VmError::InvalidOperand)?,
            );
        }
        Ok(())
    }

    /// `CopyContext dst, src` (§14.7.4.4 CreatePerIterationEnvironment).
    ///
    /// The source is read through its register after the allocation, so a
    /// collection the allocation runs cannot leave the copy reading a moved
    /// body.
    pub(crate) fn frame_copy_context(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        src_reg: u16,
    ) -> Result<(), VmError> {
        required_context(frame.read(src_reg)?)?;
        let source = frame.register_slot_ptr(src_reg)?;
        // SAFETY: the slot is a published, traced register of the running
        // frame, and holds a context for the whole call.
        let handle =
            unsafe { context::copy_context_with_roots(&mut self.gc_heap, source, &mut |_| {}) }
                .map_err(crate::oom_to_vm)?;
        frame.write(dst, Value::context(handle))
    }

    /// Copy the context held at a traced slot (the compiled tiers' rooted
    /// stub argument).
    ///
    /// # Safety
    /// `source` must point at a traced slot holding a context for the whole
    /// call.
    pub(crate) unsafe fn copy_context_from_slot(
        &mut self,
        source: *const Value,
    ) -> Result<Value, VmError> {
        // SAFETY: forwarded caller contract.
        required_context(unsafe { *source })?;
        let handle =
            unsafe { context::copy_context_with_roots(&mut self.gc_heap, source, &mut |_| {}) }
                .map_err(crate::oom_to_vm)?;
        Ok(Value::context(handle))
    }

    /// The SELF closure a module's `<module-init>` runs as: a closure over a
    /// fresh module-scope context of the init function's scope 0, or over no
    /// context when the module scope owns no slot.
    pub(crate) fn alloc_module_init_closure(
        &mut self,
        context: &ExecutionContext,
        function: &crate::CodeBlock,
    ) -> Result<Value, VmError> {
        let module_context = if function.scopes.is_empty() {
            Value::undefined()
        } else {
            self.create_context_value(context, function.id, 0, Value::undefined())?
        };
        // The context rides in the pending closure body.
        let closure = crate::closure::alloc_closure(
            &mut self.gc_heap,
            function.id,
            module_context,
            None,
            None,
        )
        .map_err(crate::oom_to_vm)?;
        Ok(Value::closure(closure))
    }

    // ------------------------------------------------------------------
    // Slot access
    // ------------------------------------------------------------------

    /// The TDZ `ReferenceError` for slot `slot` of `target`, named from its
    /// scope descriptor.
    pub(crate) fn context_slot_tdz_error(
        &self,
        context: &ExecutionContext,
        target: ContextHandle,
        slot: u16,
    ) -> VmError {
        let (function_id, scope_index) = context::scope_identity(&self.gc_heap, target);
        let described = with_scope(context, function_id, scope_index, |scope| {
            scope
                .slots
                .get(slot as usize)
                .map(|descriptor| (descriptor.kind, descriptor.name.clone()))
        });
        match described {
            Ok(Some((SlotKind::DerivedThis, _))) => {
                self.err_this_uninit(DERIVED_THIS_UNINITIALIZED.into())
            }
            Ok(Some((_, name))) => {
                self.err_this_uninit(format!("Cannot access '{name}' before initialization").into())
            }
            _ => VmError::TemporalDeadZone {
                local_index: u32::from(slot),
            },
        }
    }

    /// Resolve `coord` from `ctx` to its target context.
    fn context_target(&self, ctx: Value, coord: ContextCoord) -> Result<ContextHandle, VmError> {
        let base = required_context(ctx)?;
        context::walk(&self.gc_heap, base, coord.depth).ok_or(VmError::InvalidOperand)
    }

    /// `LoadContextSlot[Checked]` value core.
    pub(crate) fn load_context_slot_value(
        &self,
        context: &ExecutionContext,
        ctx: Value,
        coord: i32,
        checked: bool,
    ) -> Result<Value, VmError> {
        let coord = decode_coord(coord)?;
        let target = self.context_target(ctx, coord)?;
        let value =
            context::read_slot(&self.gc_heap, target, coord.slot).ok_or(VmError::InvalidOperand)?;
        if checked && value.is_hole() {
            return Err(self.context_slot_tdz_error(context, target, coord.slot));
        }
        Ok(value)
    }

    /// Raw value of the `DerivedThis` slot a `ReturnDerived` completion named;
    /// the hole while `super()` has not run.
    pub(crate) fn derived_this_slot_value(
        &self,
        ctx: Value,
        coord: otter_bytecode::ContextCoord,
    ) -> Result<Value, VmError> {
        let target = self.context_target(ctx, coord)?;
        context::read_slot(&self.gc_heap, target, coord.slot).ok_or(VmError::InvalidOperand)
    }

    /// `StoreContextSlot[Checked]` value core. The store is write-barriered.
    pub(crate) fn store_context_slot_value(
        &mut self,
        context: &ExecutionContext,
        ctx: Value,
        coord: i32,
        value: Value,
        checked: bool,
    ) -> Result<(), VmError> {
        let coord = decode_coord(coord)?;
        let target = self.context_target(ctx, coord)?;
        if checked {
            let current = context::read_slot(&self.gc_heap, target, coord.slot)
                .ok_or(VmError::InvalidOperand)?;
            if current.is_hole() {
                return Err(self.context_slot_tdz_error(context, target, coord.slot));
            }
        }
        context::write_slot(&mut self.gc_heap, target, coord.slot, value)
            .then_some(())
            .ok_or(VmError::InvalidOperand)
    }

    /// `BindThisContextSlot` value core (§9.1.1.3.1 BindThisValue).
    pub(crate) fn bind_this_context_slot_value(
        &mut self,
        ctx: Value,
        coord: i32,
        value: Value,
    ) -> Result<(), VmError> {
        let coord = decode_coord(coord)?;
        let target = self.context_target(ctx, coord)?;
        let current =
            context::read_slot(&self.gc_heap, target, coord.slot).ok_or(VmError::InvalidOperand)?;
        if !current.is_hole() {
            return Err(self.err_this_uninit(SUPER_CALLED_TWICE.into()));
        }
        context::write_slot(&mut self.gc_heap, target, coord.slot, value)
            .then_some(())
            .ok_or(VmError::InvalidOperand)
    }

    /// Assignment to a slot binding under its store `fallback`
    /// (§9.1.1.1.5 SetMutableBinding): an uninitialized binding is a
    /// `ReferenceError` before mutability is consulted.
    fn store_slot_with_fallback(
        &mut self,
        context: &ExecutionContext,
        target: ContextHandle,
        slot: u16,
        fallback: BindingStoreFallback,
        value: Value,
        name: &str,
    ) -> Result<(), VmError> {
        if fallback == BindingStoreFallback::ImmutableIgnore {
            return Ok(());
        }
        let current =
            context::read_slot(&self.gc_heap, target, slot).ok_or(VmError::InvalidOperand)?;
        if current.is_hole() {
            return Err(self.context_slot_tdz_error(context, target, slot));
        }
        match fallback {
            BindingStoreFallback::Mutable => {
                context::write_slot(&mut self.gc_heap, target, slot, value)
                    .then_some(())
                    .ok_or(VmError::InvalidOperand)
            }
            BindingStoreFallback::ImmutableThrow => {
                Err(self.err_type(format!("Assignment to constant variable '{name}'.").into()))
            }
            BindingStoreFallback::ImmutableIgnore => Ok(()),
        }
    }

    // ------------------------------------------------------------------
    // Eval-extension lookups
    // ------------------------------------------------------------------

    fn lookup_name(
        context: &ExecutionContext,
        function_id: u32,
        name_idx: u32,
    ) -> Result<&str, VmError> {
        context
            .string_constant_str_for_function(function_id, name_idx)
            .ok_or(VmError::InvalidOperand)
    }

    /// `LoadLookupSlot` value core: an extension hit at hops `[0, depth)`,
    /// else the TDZ-checked slot.
    pub(crate) fn load_lookup_slot_value(
        &self,
        context: &ExecutionContext,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        coord: i32,
    ) -> Result<Value, VmError> {
        let decoded = decode_coord(coord)?;
        let name = Self::lookup_name(context, function_id, name_idx)?;
        let base = required_context(ctx)?;
        if let Some(extension) =
            context::probe_extensions(&self.gc_heap, Some(base), decoded.depth, name)
        {
            return eval_env::extension_get(&self.gc_heap, extension, name)
                .ok_or(VmError::InvalidOperand);
        }
        self.load_context_slot_value(context, ctx, coord, true)
    }

    /// `StoreLookupSlot` value core: an extension hit writes the entry, a
    /// miss applies `fallback` to the slot.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_lookup_slot_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        coord: i32,
        fallback: i32,
        value: Value,
    ) -> Result<(), VmError> {
        let coord = decode_coord(coord)?;
        let fallback = BindingStoreFallback::from_imm32(fallback).ok_or(VmError::InvalidOperand)?;
        let name = Self::lookup_name(context, function_id, name_idx)?;
        let base = required_context(ctx)?;
        if let Some(extension) =
            context::probe_extensions(&self.gc_heap, Some(base), coord.depth, name)
        {
            eval_env::extension_set_existing(&mut self.gc_heap, extension, name, value);
            return Ok(());
        }
        let target =
            context::walk(&self.gc_heap, base, coord.depth).ok_or(VmError::InvalidOperand)?;
        self.store_slot_with_fallback(context, target, coord.slot, fallback, value, name)
    }

    /// `DeleteLookupSlot` value core: an extension hit is removed (`true`);
    /// a miss leaves the declarative slot intact (`false`).
    pub(crate) fn delete_lookup_slot_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        depth: i32,
    ) -> Result<Value, VmError> {
        let depth = decode_depth(depth)?;
        let name = Self::lookup_name(context, function_id, name_idx)?;
        let removed = context::probe_extensions(&self.gc_heap, context_operand(ctx)?, depth, name)
            .is_some_and(|extension| {
                eval_env::extension_remove(&mut self.gc_heap, extension, name)
            });
        Ok(Value::boolean(removed))
    }

    /// `LoadLookupGlobal` / `TypeofLookupGlobal` value core.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn load_lookup_global_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        depth: i32,
        missing: BindingMissing,
    ) -> Result<Value, CommittedValueError> {
        let depth = decode_depth(depth).map_err(CommittedValueError::Fatal)?;
        let name = Self::lookup_name(context, function_id, name_idx)
            .map_err(CommittedValueError::Fatal)?;
        if let Some(extension) = context::probe_extensions(
            &self.gc_heap,
            context_operand(ctx).map_err(CommittedValueError::Fatal)?,
            depth,
            name,
        ) {
            return eval_env::extension_get(&self.gc_heap, extension, name)
                .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand));
        }
        match missing {
            BindingMissing::Throw => {
                self.load_global_or_throw_value(stack, context, function_id, name_idx)
            }
            BindingMissing::Undefined => {
                self.load_global_or_undefined_value(context, stack, function_id, name_idx)
            }
        }
    }

    /// `StoreLookupGlobal` value core.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_lookup_global_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        mode: i32,
        value: Value,
    ) -> Result<(), CommittedValueError> {
        let mode = LookupGlobalMode::from_imm32(mode)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let name = Self::lookup_name(context, function_id, name_idx)
            .map_err(CommittedValueError::Fatal)?;
        if let Some(extension) = context::probe_extensions(
            &self.gc_heap,
            context_operand(ctx).map_err(CommittedValueError::Fatal)?,
            mode.depth,
            name,
        ) {
            eval_env::extension_set_existing(&mut self.gc_heap, extension, name, value);
            return Ok(());
        }
        self.store_global_binding_value(context, stack, function_id, value, name_idx, mode.strict)
    }

    /// `DeleteLookupGlobal` value core: an extension hit is removed, else
    /// the global delete runs (a global lexical yields `false`).
    pub(crate) fn delete_lookup_global_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        depth: i32,
    ) -> Result<Value, VmError> {
        let depth = decode_depth(depth)?;
        let name = Self::lookup_name(context, function_id, name_idx)?;
        if let Some(extension) =
            context::probe_extensions(&self.gc_heap, context_operand(ctx)?, depth, name)
        {
            let removed = eval_env::extension_remove(&mut self.gc_heap, extension, name);
            return Ok(Value::boolean(removed));
        }
        let removed = if self.global_lexicals.contains_key(name) {
            // A global declarative binding is an Environment Record binding,
            // not a configurable property: `delete identifier` is `false`.
            false
        } else {
            crate::object::delete(&mut self.global_this, &mut self.gc_heap, name)?
        };
        Ok(Value::boolean(removed))
    }

    /// `ResolveLookupRef` value core (§13.15.2: the reference base resolves
    /// before the right-hand side runs). Yields the extension holding the
    /// name, else the slot's target context, else `undefined` (global).
    pub(crate) fn resolve_lookup_ref_value(
        &self,
        context: &ExecutionContext,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        target: i32,
    ) -> Result<Value, VmError> {
        let target = LookupRefTarget::from_imm32(target);
        let name = Self::lookup_name(context, function_id, name_idx)?;
        let base = context_operand(ctx)?;
        if let Some(extension) =
            context::probe_extensions(&self.gc_heap, base, target.depth(), name)
        {
            return Ok(Value::eval_extension(extension));
        }
        match target {
            LookupRefTarget::Slot(coord) => {
                let base = base.ok_or(VmError::InvalidOperand)?;
                let target = context::walk(&self.gc_heap, base, coord.depth)
                    .ok_or(VmError::InvalidOperand)?;
                Ok(Value::context(target))
            }
            LookupRefTarget::Global { .. } => Ok(Value::undefined()),
        }
    }

    /// `StoreRef` value core: PutValue through the base
    /// [`Self::resolve_lookup_ref_value`] produced.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_ref_value(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        function_id: u32,
        reference: Value,
        name_idx: u32,
        mode: i32,
        value: Value,
    ) -> Result<(), CommittedValueError> {
        let mode = StoreRefMode::from_imm32(mode)
            .ok_or(VmError::InvalidOperand)
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let name = Self::lookup_name(context, function_id, name_idx)
            .map_err(CommittedValueError::Fatal)?;
        if let Some(extension) = reference.as_eval_extension() {
            if eval_env::extension_set_existing(&mut self.gc_heap, extension, name, value) {
                return Ok(());
            }
            // §9.1.1.1.5 step 1: the right-hand side deleted the binding.
            if mode.strict {
                return Err(CommittedValueError::JavaScript(
                    self.err_undefined_ident(name.into()),
                ));
            }
            eval_env::extension_set_or_insert(&mut self.gc_heap, extension, name, value);
            return Ok(());
        }
        if let Some(target) = reference.as_context() {
            let slot = mode
                .slot
                .ok_or(VmError::InvalidOperand)
                .map_err(|error| CommittedValueError::Fatal(error.into()))?;
            return self
                .store_slot_with_fallback(context, target, slot, mode.fallback, value, name)
                .map_err(|error| CommittedValueError::JavaScript(error.into()));
        }
        if reference.is_undefined() {
            return self.store_global_binding_value(
                context,
                stack,
                function_id,
                value,
                name_idx,
                mode.strict,
            );
        }
        Err(CommittedValueError::Fatal(VmError::InvalidOperand))
    }

    /// The eval extension of the context at hop `var_depth` from `*ctx`,
    /// created when absent. `*ctx` and `*value` are rooted across the
    /// extension allocation and re-read after it.
    fn var_scope_extension(
        &mut self,
        ctx: &mut Value,
        value: &mut Value,
        var_depth: u16,
    ) -> Result<eval_env::EvalExtensionHandle, VmError> {
        let target = context::walk(&self.gc_heap, required_context(*ctx)?, var_depth)
            .ok_or(VmError::InvalidOperand)?;
        if let Some(extension) = context::extension(&self.gc_heap, target) {
            return Ok(extension);
        }
        let ctx_slot: *mut Value = ctx;
        let value_slot: *mut Value = value;
        let extension = eval_env::alloc_extension_with_roots(&mut self.gc_heap, &mut |visitor| {
            // SAFETY: both locals outlive the allocation and are rewritten in
            // place by the collector.
            unsafe {
                (*ctx_slot).trace_value_slot_mut(visitor);
                (*value_slot).trace_value_slot_mut(visitor);
            }
        })
        .map_err(crate::oom_to_vm)?;
        // The chain may have moved: walk again from the rewritten root.
        let target = context::walk(&self.gc_heap, required_context(*ctx)?, var_depth)
            .ok_or(VmError::InvalidOperand)?;
        context::set_extension(&mut self.gc_heap, target, extension);
        Ok(extension)
    }

    /// `DeclareEvalVar` value core: create `name = undefined` in the var
    /// scope's extension unless it already binds the name.
    pub(crate) fn declare_eval_var_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        var_depth: i32,
    ) -> Result<(), VmError> {
        let var_depth = decode_depth(var_depth)?;
        let name = Self::lookup_name(context, function_id, name_idx)?;
        let mut ctx = ctx;
        let mut unused = Value::undefined();
        let extension = self.var_scope_extension(&mut ctx, &mut unused, var_depth)?;
        eval_env::extension_insert_absent(&mut self.gc_heap, extension, name);
        Ok(())
    }

    /// `StoreVarScope` value core: set-or-create `name` in the var scope's
    /// extension.
    pub(crate) fn store_var_scope_value(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        ctx: Value,
        name_idx: u32,
        var_depth: i32,
        value: Value,
    ) -> Result<(), VmError> {
        let var_depth = decode_depth(var_depth)?;
        let name = Self::lookup_name(context, function_id, name_idx)?;
        let mut ctx = ctx;
        let mut value = value;
        let extension = self.var_scope_extension(&mut ctx, &mut value, var_depth)?;
        eval_env::extension_set_or_insert(&mut self.gc_heap, extension, name, value);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Direct eval
    // ------------------------------------------------------------------

    /// The context chain a direct eval at `ctx` compiles against: every live
    /// context from `ctx` outward with its descriptor and current extension
    /// names, and the hop count of the first VariableEnvironment context
    /// (`None`: the global environment).
    pub(crate) fn eval_caller_chain(
        &self,
        context: &ExecutionContext,
        ctx: Value,
    ) -> Result<EvalCallerChain, VmError> {
        let mut chain = EvalCallerChain::default();
        let mut current = context_operand(ctx)?;
        while let Some(handle) = current {
            let (function_id, scope_index) = context::scope_identity(&self.gc_heap, handle);
            let descriptor = with_scope(context, function_id, scope_index, Clone::clone)?;
            if chain.var_depth.is_none() && descriptor.flags.var_scope {
                chain.var_depth =
                    Some(u32::try_from(chain.scopes.len()).map_err(|_| VmError::InvalidOperand)?);
            }
            let extension_names = context::extension(&self.gc_heap, handle)
                .map(|extension| eval_env::extension_names(&self.gc_heap, extension))
                .unwrap_or_default();
            chain.scopes.push(EvalCallerScope {
                descriptor,
                extension_names,
            });
            current = context::parent(&self.gc_heap, handle);
        }
        Ok(chain)
    }
}

#[cfg(test)]
#[path = "context_ops/tests.rs"]
mod tests;
