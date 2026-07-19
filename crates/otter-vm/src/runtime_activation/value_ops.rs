//! Value, binding, property, element, and allocation operations.
//!
//! Each method owns its short [`crate::ActiveFrameMut`] scope internally. The
//! JIT supplies decoded operands and receives semantic results; no VM service
//! or frame/window representation crosses the boundary.

use crate::{NumericRuntimeOp, UnaryCoercionOp, UnaryPrimitiveHint, Value, VmError};

use super::{RuntimeCall, RuntimeFrameIdentity};

impl RuntimeCall<'_> {
    /// Load one realm builtin error constructor.
    pub fn load_builtin_error(&mut self, dst: u16, kind_index: u32) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall exclusively owns this validated descriptor.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_load_builtin_error(context, &mut frame, dst, kind_index)
    }

    /// Define one object-literal data property through the current activation.
    pub fn define_data_property(
        &mut self,
        object: u16,
        key: u16,
        value: u16,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall owns the canonical published descriptor.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_define_data_property(stack, context, &mut frame, object, key, value)
    }

    /// Apply an accessor-aware property descriptor through the current
    /// canonical activation.
    pub fn define_own_property(
        &mut self,
        target: u16,
        key: u16,
        descriptor: u16,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall exclusively owns this validated descriptor.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_define_own_property(stack, context, &mut frame, target, key, descriptor)
    }

    /// Complete a guarded Math call through the current activation.
    pub fn math_call(
        &mut self,
        dst: u16,
        method_id: u32,
        argument_regs: &[u16],
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall exclusively owns this validated descriptor.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_math_call(stack, context, &mut frame, dst, method_id, argument_regs)
    }

    /// Materialize a string constant into the current activation.
    pub fn load_string(
        &mut self,
        function_id: u32,
        dst: u16,
        constant_index: u32,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall owns the published descriptor for this operation.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_load_string(context, &mut frame, function_id, dst, constant_index)
    }

    /// Allocate a closure from the current activation's captured-cell window.
    ///
    /// Materialized entries retain their cold lexical sidecar. Generated
    /// stack-owned callees use the canonical native window and never synthesize
    /// a [`crate::Frame`].
    pub fn make_closure(
        &mut self,
        function_id: u32,
        dst: u16,
        function_index: u32,
        parent_indices: &[u32],
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        match self.identity {
            RuntimeFrameIdentity::Materialized(index) => vm.jit_runtime_make_closure(
                context,
                unsafe { &mut *self.stack.as_ptr() },
                index as usize,
                function_id,
                dst,
                function_index,
                parent_indices,
            ),
            RuntimeFrameIdentity::StackOwned => {
                let frame = self.frame.as_ptr();
                // SAFETY: RuntimeCall validated and exclusively owns this
                // descriptor for the duration of the semantic operation.
                let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
                    .map_err(|_| VmError::InvalidOperand)?;
                vm.jit_runtime_make_closure_native(
                    context,
                    &mut frame,
                    function_id,
                    dst,
                    function_index,
                    parent_indices,
                )
            }
        }
    }

    /// Allocate one capture-free function value through the current
    /// activation without requiring a materialized interpreter frame.
    pub fn make_function(
        &mut self,
        function_id: u32,
        dst: u16,
        function_index: u32,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        match self.identity {
            RuntimeFrameIdentity::Materialized(index) => vm.jit_runtime_make_function(
                context,
                unsafe { &mut *self.stack.as_ptr() },
                index as usize,
                dst,
                function_index,
            ),
            RuntimeFrameIdentity::StackOwned => {
                let frame = self.frame.as_ptr();
                // SAFETY: RuntimeCall exclusively owns the validated published
                // descriptor for this semantic operation.
                let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
                    .map_err(|_| VmError::InvalidOperand)?;
                vm.jit_runtime_make_function_native(
                    context,
                    &mut frame,
                    function_id,
                    dst,
                    function_index,
                )
            }
        }
    }

    /// Complete generic ECMAScript addition.
    pub fn add(&mut self, dst: u16, lhs: u16, rhs: u16) -> Result<(), VmError> {
        // SAFETY: RuntimeCall brands exclusive mutator access for this exact
        // operation; neither reference is retained by the raw frame view.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall construction validated and exclusively owns the
        // published descriptor; ActiveFrame stores no borrowed register slice.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_add(&mut frame, dst, lhs, rhs)
    }

    /// Complete generic unary negation.
    pub fn neg(&mut self, dst: u16, src: u16) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_neg(&mut frame, dst, src)
    }

    /// Complete one decoded numeric-family operation.
    pub fn numeric(
        &mut self,
        dst: u16,
        lhs: u16,
        operation: NumericRuntimeOp,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_numeric_op(stack, context, &mut frame, dst, lhs, operation)
    }

    /// Complete one decoded unary coercion.
    pub fn coerce_unary(
        &mut self,
        dst: u16,
        src: u16,
        operation: UnaryCoercionOp,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_coerce_unary(stack, context, &mut frame, dst, src, operation)
    }

    /// Resolve a `ToPrimitive` hint in the current function and complete the
    /// coercion without exposing the execution context to the JIT.
    pub fn coerce_unary_hint(
        &mut self,
        dst: u16,
        src: u16,
        hint_index: u32,
    ) -> Result<(), VmError> {
        // SAFETY: immutable context is live for the branded call extent; the
        // returned token is consumed before any VM transition.
        let token = unsafe { self.context.as_ref() }
            .string_constant_str_for_function(self.function_id(), hint_index)
            .ok_or(VmError::InvalidOperand)?;
        let hint = UnaryPrimitiveHint::from_token(token).ok_or(VmError::InvalidOperand)?;
        self.coerce_unary(dst, src, UnaryCoercionOp::ToPrimitive { hint })
    }

    /// Allocate and commit an array from decoded source registers.
    pub fn new_array(&mut self, dst: u16, sources: &[u16]) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_new_array(&mut frame, dst, sources)
    }

    /// Allocate and commit an ordinary object.
    pub fn new_object(&mut self, dst: u16) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_new_object(&mut frame, dst)
    }

    /// Store a captured binding with its TDZ check.
    pub fn store_upvalue_checked(&mut self, src: u16, index: i32) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_store_upvalue_checked(&mut frame, src, index)
    }

    /// Replace a loop-captured upvalue with a fresh cell.
    pub fn fresh_upvalue(&mut self, index: i32) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_fresh_upvalue(&mut frame, index)
    }

    /// Read a captured binding.
    pub fn load_upvalue(&mut self, dst: u16, index: i32) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_load_upvalue(&mut frame, dst, index)
    }

    /// Write a captured binding.
    pub fn store_upvalue(&mut self, src: u16, index: i32) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_store_upvalue(&mut frame, src, index)
    }

    /// Load one computed element.
    pub fn load_element(&mut self, dst: u16, recv: u16, index: u16) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_load_element(context, &mut frame, stack, dst, recv, index)
    }

    /// Load one global binding through the owning function's constant pool.
    pub fn load_global(
        &mut self,
        function_id: u32,
        dst: u16,
        name_index: u32,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        let stack = unsafe { &mut *self.stack.as_ptr() };
        vm.jit_runtime_load_global(stack, context, &mut frame, function_id, dst, name_index)
    }

    /// Materialize a regular-expression literal.
    pub fn load_regexp(&mut self, dst: u16, index: u32) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::add`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_load_regexp(context, &mut frame, dst, index)
    }

    /// Run the generational write barrier for one property store.
    pub fn write_barrier(&mut self, object: u16, source: u16) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall construction validated the shared descriptor.
        let frame = unsafe { crate::ActiveFrameRef::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_write_barrier(&frame, object, source)
    }

    /// Resolve and complete one named-property read miss.
    #[allow(clippy::too_many_arguments)]
    pub fn load_property(
        &mut self,
        function_id: u32,
        dst: u16,
        object: u16,
        name_index: u32,
        site: usize,
    ) -> Result<u64, VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall owns the descriptor. Its raw slot descriptors do
        // not borrow the materialized stack and stay scoped to this operation.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_load_property(
            context,
            &mut frame,
            stack,
            function_id,
            dst,
            object,
            name_index,
            site,
        )
    }

    /// Resolve and complete one named-property write miss.
    #[allow(clippy::too_many_arguments)]
    pub fn store_property(
        &mut self,
        function_id: u32,
        object: u16,
        name_index: u32,
        source: u16,
        site: usize,
    ) -> Result<u64, VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: as [`Self::load_property`].
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_store_property(
            context,
            &mut frame,
            stack,
            function_id,
            object,
            name_index,
            source,
            site,
        )
    }

    /// Store one computed element through the representation-neutral frame.
    /// The VM's value-level `[[Set]]` funnel completes every receiver kind
    /// synchronously, including typed arrays, proxies, and callable setters.
    pub fn store_element(&mut self, recv: u16, index: u16, source: u16) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall validated the raw descriptor and retains its
        // owner. The frame view is used only to copy the four scalar inputs;
        // the VM helper receives no frame reference.
        let frame = unsafe { crate::ActiveFrameRef::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_store_element(stack, context, &frame, recv, index, source)
    }

    /// Commit a value without exposing the destination window.
    pub fn commit(&mut self, dst: u16, value: Value) -> Result<(), VmError> {
        self.write(dst, value)
    }
}
