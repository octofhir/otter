//! Value, binding, property, element, and allocation operations.
//!
//! Register-index methods own a short [`crate::ActiveFrameMut`] scope
//! internally. Fixed-value methods instead use the published frame only for
//! function/PC identity and operate on explicitly rooted boxed operands. The
//! JIT supplies decoded inputs and receives semantic results; no borrowed
//! frame/window representation crosses the VM service boundary.

use smallvec::SmallVec;

use crate::{NumericRuntimeOp, UnaryCoercionOp, UnaryPrimitiveHint, Value, VmError};

use super::{RuntimeCall, RuntimeFrameIdentity};

impl RuntimeCall<'_> {
    /// Bind `super()`'s result into a stack-owned derived constructor without
    /// materializing an interpreter frame.
    pub fn bind_derived_this(&mut self, src: u16) -> Result<(), VmError> {
        if self.identity != RuntimeFrameIdentity::StackOwned {
            return Err(VmError::InvalidOperand);
        }
        let value = self.read(src)?;
        self.bind_derived_this_value(value)
    }

    /// Value-form of [`Self::bind_derived_this`] used by Machine IR after the
    /// `super` result has left its bytecode register identity.
    pub fn bind_derived_this_value(&mut self, value: Value) -> Result<(), VmError> {
        if let RuntimeFrameIdentity::Materialized(frame_index) = self.identity {
            let vm = unsafe { &mut *self.vm.as_ptr() };
            let stack = unsafe { &mut *self.stack.as_ptr() };
            vm.run_bind_this_value(stack, frame_index as usize, value)?;
            return self.refresh_materialized_this_value();
        }
        let frame_ptr = self.frame.as_ptr();
        // SAFETY: RuntimeCall exclusively owns the validated published frame.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame_ptr) }
            .map_err(|_| VmError::InvalidOperand)?;
        let flags = frame.header().flags;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        if !flags.contains(crate::NativeFrameFlags::DERIVED_CONSTRUCTOR) {
            return Err(vm.err_this_uninit(
                "super called outside a derived constructor"
                    .to_string()
                    .into(),
            ));
        }
        if !frame.this_value().is_hole() {
            return Err(vm.err_this_uninit(
                "super constructor may only be called once"
                    .to_string()
                    .into(),
            ));
        }
        frame.set_this_value(value);
        Ok(())
    }

    /// Mirror a committed materialized derived-`this` binding into the native
    /// descriptor used by the rest of the current compiled entry.
    pub fn refresh_materialized_this_value(&mut self) -> Result<(), VmError> {
        let RuntimeFrameIdentity::Materialized(frame_index) = self.identity else {
            return Ok(());
        };
        let stack = unsafe { self.stack.as_ref() };
        let value = stack
            .get(frame_index as usize)
            .ok_or(VmError::InvalidOperand)?
            .this_value;
        self.with_frame(|frame| {
            frame.set_this_value(value);
            Ok(())
        })
    }

    /// Complete a generic method-call guard miss on either physical frame
    /// representation and commit the result without materializing the caller.
    pub fn call_method(
        &mut self,
        dst: u16,
        receiver: u16,
        name_index: u32,
        argument_regs: &[u16],
    ) -> Result<(), VmError> {
        let receiver = self.read(receiver)?;
        let mut args = SmallVec::with_capacity(argument_regs.len());
        for &register in argument_regs {
            args.push(self.read(register)?);
        }
        let function_id = self.function_id();
        let call_pc = self.pc();
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let result = vm.jit_runtime_method_call_values(
            context,
            stack,
            function_id,
            call_pc,
            receiver,
            name_index,
            args,
        )?;
        self.write(dst, result)
    }

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
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall construction validated and exclusively owns the
        // published descriptor; ActiveFrame stores no borrowed register slice.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_add(stack, context, &mut frame, dst, lhs, rhs)
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

    /// Read a captured binding as an SSA value without assigning a bytecode
    /// destination register.
    pub fn load_upvalue_value(&self, index: i32) -> Result<Value, VmError> {
        let index = u32::try_from(index).map_err(|_| VmError::InvalidOperand)?;
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall owns the validated published descriptor.
        let frame = unsafe { crate::ActiveFrameRef::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        let value =
            crate::read_upvalue(unsafe { &self.vm.as_ref().gc_heap }, frame.upvalue(index)?);
        if value.is_hole() {
            Err(VmError::TemporalDeadZone { local_index: index })
        } else {
            Ok(value)
        }
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

    /// Complete one computed `[[Get]]` over boxed SSA values.
    ///
    /// The published frame supplies only the exact feedback identity and GC
    /// root map. Operand words are JavaScript values, never register indices,
    /// and the operation returns its value directly without frame replay.
    pub fn load_element_value(&mut self, receiver: Value, key: Value) -> Result<Value, VmError> {
        let function_id = self.function_id();
        let instruction_pc = self.pc();
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        vm.jit_runtime_load_element_value(
            stack,
            context,
            function_id,
            instruction_pc,
            receiver,
            key,
        )
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
    ) -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
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
    ) -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
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

    /// Complete one computed `[[Set]]` over boxed SSA values.
    ///
    /// The fixed-value boundary covers arrays, typed arrays, proxies, and
    /// callable setters. Success means the store committed exactly once;
    /// failures are returned for the native entry to park as a throw.
    pub fn store_element_value(
        &mut self,
        receiver: Value,
        key: Value,
        value: Value,
    ) -> Result<(), VmError> {
        let function_id = self.function_id();
        let instruction_pc = self.pc();
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        vm.jit_runtime_store_element_value(
            stack,
            context,
            function_id,
            instruction_pc,
            receiver,
            key,
            value,
        )
    }

    /// Commit a value without exposing the destination window.
    pub fn commit(&mut self, dst: u16, value: Value) -> Result<(), VmError> {
        self.write(dst, value)
    }
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use otter_bytecode::{BytecodeModule, Function, Instruction, Op, Operand, SourceKind};

    use crate::{
        ActivationStack, ExecutionContext, Interpreter, JitElementFamily, NativeFrame,
        NativeFrameKind, Value, VmFrameHeader, VmRuntimeActivation,
    };

    use super::RuntimeCall;

    fn element_context() -> ExecutionContext {
        let element_operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::Register(2),
        ];
        ExecutionContext::from_module(BytecodeModule {
            module: "runtime-value-element-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "elementBoundary".to_string(),
                locals: 3,
                code: vec![
                    Instruction {
                        pc: 0,
                        op: Op::LoadElement,
                        operands: element_operands.clone(),
                    },
                    Instruction {
                        pc: 1,
                        op: Op::LoadElement,
                        operands: element_operands,
                    },
                    Instruction {
                        pc: 2,
                        op: Op::ReturnUndefined,
                        operands: Vec::new(),
                    },
                ]
                .into(),
                ..Function::default()
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    #[test]
    fn stack_owned_value_elements_use_published_feedback_identity() {
        let context = element_context();
        let mut vm = Interpreter::new();
        let array =
            crate::array::from_elements_old_for_fixture(&mut vm.gc_heap, [Value::number_f64(4.0)])
                .expect("packed array fixture");
        let receiver = Value::array(array);
        let key = Value::number_i32(0);
        let mut stack = ActivationStack::new();
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [Value::undefined(); 3];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                code_block_id: 0,
                pc: 1,
                register_count: 3,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();

        {
            // SAFETY: activation, native frame, and its register window remain
            // live and exclusively owned for this RuntimeCall scope.
            let mut call = unsafe {
                RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame))
            }
            .expect("stack-owned runtime call");
            assert_eq!(
                call.load_element_value(receiver, key)
                    .expect("value load")
                    .as_f64(),
                Some(4.0)
            );
            call.store_element_value(receiver, key, Value::number_f64(9.5))
                .expect("value store");
            assert_eq!(
                call.load_element_value(receiver, key)
                    .expect("updated value load")
                    .as_f64(),
                Some(9.5)
            );
        }

        let code_block = context.exec_function(0).expect("test function");
        assert_eq!(
            code_block
                .feedback_at(0)
                .expect("cold feedback cell")
                .element_family(),
            JitElementFamily::Unseen
        );
        assert_eq!(
            code_block
                .feedback_at(1)
                .expect("published-PC feedback cell")
                .element_family(),
            JitElementFamily::DenseFloat64
        );
        assert_eq!(code_block.feedback_epoch(), 1);
    }
}
