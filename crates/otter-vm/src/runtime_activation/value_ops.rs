//! Value, binding, property, element, and allocation operations.
//!
//! Register-index methods own a short [`crate::ActiveFrameMut`] scope
//! internally. Fixed-value methods instead use the published frame only for
//! function/PC identity and operate on explicitly rooted boxed operands. The
//! JIT supplies decoded inputs and receives semantic results; no borrowed
//! frame/window representation crosses the VM service boundary.

use smallvec::SmallVec;

use crate::{
    CommittedValueError, NumericRuntimeOp, UnaryCoercionOp, UnaryPrimitiveHint, Value, VmError,
};

use super::{RuntimeCall, RuntimeFrameIdentity};

impl RuntimeCall<'_> {
    /// Bind the boxed `super()` result carried by the committed-value boundary.
    pub fn bind_derived_this_value(&mut self, value: Value) -> Result<(), CommittedValueError> {
        if let RuntimeFrameIdentity::Materialized(frame_index) = self.identity {
            let frame_ptr = self.frame.as_ptr();
            let stack = unsafe { &mut *self.stack.as_ptr() };
            let vm = unsafe { &mut *self.vm.as_ptr() };
            vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
            vm.record_jit_derived_this_bind_transition();
            let bound_this = match vm.bind_this_value(stack, frame_index, value) {
                Ok(bound_this) => bound_this,
                Err(error @ VmError::ThisUninitialized) => {
                    return Err(CommittedValueError::JavaScript(error));
                }
                Err(error) => return Err(CommittedValueError::Fatal(error)),
            };
            // SAFETY: the descriptor was validated before the non-allocating,
            // non-reentrant bind and remains published by the RuntimeCall.
            let mut native = unsafe { crate::ActiveFrameMut::from_native_ptr(frame_ptr) }
                .expect("published materialized frame remains valid after bind-only kernel");
            native.set_this_value(bound_this);
            return Ok(());
        }
        let frame_ptr = self.frame.as_ptr();
        // SAFETY: RuntimeCall exclusively owns the validated published frame;
        // validation and frame-kind classification precede semantic entry.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame_ptr) }
            .map_err(|_| CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let flags = frame.header().flags;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        if !flags.contains(crate::native_abi::NativeFrameFlags::DERIVED_CONSTRUCTOR) {
            return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
        }
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        vm.record_jit_derived_this_bind_transition();
        if !frame.this_value().is_hole() {
            return Err(CommittedValueError::JavaScript(
                vm.err_this_uninit(
                    "super constructor may only be called once"
                        .to_string()
                        .into(),
                ),
            ));
        }
        frame.set_this_value(value);
        Ok(())
    }

    /// Complete the exact published `New` from boxed SSA values: `values[0]`
    /// is the constructor and the rest every actual argument. `new.target`
    /// is the constructor itself, as for any direct `new` expression.
    pub fn construct_values(&mut self, values: &[Value]) -> Result<Value, VmError> {
        let (callee, args) = values.split_first().ok_or(VmError::InvalidOperand)?;
        let function_id = self.function_id();
        let call_pc = self.pc();
        let context = unsafe { self.context.as_ref() };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(call_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if instruction.instruction_pc != call_pc
            || function.op(instruction) != otter_bytecode::Op::New
        {
            return Err(VmError::InvalidOperand);
        }
        let argument_count = function
            .const_index(instruction, 2)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(VmError::InvalidOperand)?;
        if args.len() != argument_count {
            return Err(VmError::InvalidOperand);
        }
        let args = SmallVec::from_slice(args);
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        vm.jit_runtime_construct_values(context, stack, function_id, call_pc, *callee, args)
    }

    /// Complete the exact published explicit-receiver call from boxed SSA
    /// values: `values[0]` is the callee, `values[1]` the receiver, and the
    /// rest every actual argument. The site is a `CallWithThis`, or a plain
    /// `Call` whose generated caller supplies `undefined` as the receiver; the
    /// callee's own binding rules apply either way.
    pub fn call_with_this_values(&mut self, values: &[Value]) -> Result<Value, VmError> {
        let (callee, rest) = values.split_first().ok_or(VmError::InvalidOperand)?;
        let (receiver, args) = rest.split_first().ok_or(VmError::InvalidOperand)?;
        let function_id = self.function_id();
        let call_pc = self.pc();
        let context = unsafe { self.context.as_ref() };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(call_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        let count_operand = match function.op(instruction) {
            otter_bytecode::Op::CallWithThis => 3,
            otter_bytecode::Op::Call => 2,
            _ => return Err(VmError::InvalidOperand),
        };
        if instruction.instruction_pc != call_pc {
            return Err(VmError::InvalidOperand);
        }
        let argument_count = function
            .const_index(instruction, count_operand)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(VmError::InvalidOperand)?;
        if args.len() != argument_count {
            return Err(VmError::InvalidOperand);
        }
        let args = SmallVec::from_slice(args);
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        vm.jit_runtime_call_with_this_values(
            context,
            stack,
            function_id,
            call_pc,
            *callee,
            *receiver,
            args,
        )
    }

    /// Complete the exact published `CallMethodValue` from boxed SSA values.
    ///
    /// `values[0]` is the receiver and the remaining values are the complete
    /// actual argument list. The immutable CodeBlock is authoritative for the
    /// method name and argument count; validating both before entering the VM
    /// keeps this boundary effect-once even when lookup or the callee throws.
    pub fn call_method_values(&mut self, values: &[Value]) -> Result<Value, VmError> {
        let (receiver, args) = values.split_first().ok_or(VmError::InvalidOperand)?;
        let function_id = self.function_id();
        let call_pc = self.pc();
        let context = unsafe { self.context.as_ref() };
        let function = context
            .exec_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let instruction = function
            .instr_at_index(call_pc as usize)
            .ok_or(VmError::InvalidOperand)?;
        if instruction.instruction_pc != call_pc
            || function.op(instruction) != otter_bytecode::Op::CallMethodValue
        {
            return Err(VmError::InvalidOperand);
        }
        let name_index = function
            .const_index(instruction, 2)
            .ok_or(VmError::InvalidOperand)?;
        let argument_count = function
            .const_index(instruction, 3)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(VmError::InvalidOperand)?;
        if args.len() != argument_count {
            return Err(VmError::InvalidOperand);
        }

        // The native entry has already copied the machine-owned packet. Keep
        // an owned inline vector for the existing VM anchor so no borrowed JIT
        // stack memory survives allocation or JavaScript reentry.
        let args = SmallVec::from_slice(args);
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        vm.jit_runtime_method_call_values(
            context,
            stack,
            function_id,
            call_pc,
            *receiver,
            name_index,
            args,
        )
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

    /// Allocate a closure from the current published activation.
    pub fn make_closure(
        &mut self,
        function_id: u32,
        dst: u16,
        function_index: u32,
        parent_indices: &[u32],
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall validated and exclusively owns this descriptor
        // for the duration of the semantic operation.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_make_closure(
            context,
            &mut frame,
            function_id,
            dst,
            function_index,
            parent_indices,
        )
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
        let frame = self.frame.as_ptr();
        // SAFETY: RuntimeCall exclusively owns the validated published
        // descriptor for this semantic operation.
        let mut frame = unsafe { crate::ActiveFrameMut::from_native_ptr(frame) }
            .map_err(|_| VmError::InvalidOperand)?;
        vm.jit_runtime_make_function(context, &mut frame, function_id, dst, function_index)
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

    /// Complete the named-property read identified by the published frame.
    ///
    /// The caller supplies one boxed receiver rather than register indices.
    /// Function id, logical PC, property name, and feedback site are decoded
    /// and validated against the immutable CodeBlock by the VM.
    pub fn load_property_value(
        &mut self,
        receiver: Value,
    ) -> Result<(Value, Option<crate::jit::JitPropertyIcWay>), VmError> {
        let function_id = self.function_id();
        let instruction_pc = self.pc();
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        vm.jit_runtime_load_property_value(stack, context, function_id, instruction_pc, receiver)
    }

    /// Complete the named-property write identified by the published frame.
    ///
    /// Success means the complete store committed exactly once and returns an
    /// optional inline-cache program for the compiler-owned cell.
    pub fn store_property_value(
        &mut self,
        receiver: Value,
        value: Value,
    ) -> Result<Option<crate::jit::JitPropertyIcWay>, VmError> {
        let function_id = self.function_id();
        let instruction_pc = self.pc();
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        vm.jit_runtime_store_property_value(
            stack,
            context,
            function_id,
            instruction_pc,
            receiver,
            value,
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

    use otter_bytecode::{
        BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind,
    };

    use crate::{
        ActivationStack, ExecutionContext, Interpreter, JitElementFamily, Value, VmError,
        VmRuntimeActivation,
        native_abi::{NativeFrame, NativeFrameKind, VmFrameHeader},
        rooting::RootScopeExt,
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
            function_source: None,
        })
        .expect("valid bytecode fixture")
    }

    fn named_property_module() -> BytecodeModule {
        BytecodeModule {
            module: "runtime-value-property-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "propertyBoundary".to_string(),
                locals: 4,
                code: vec![
                    Instruction {
                        pc: 0,
                        op: Op::LoadProperty,
                        operands: vec![
                            Operand::Register(0),
                            Operand::Register(1),
                            Operand::ConstIndex(0),
                        ],
                    },
                    Instruction {
                        pc: 1,
                        op: Op::LoadProperty,
                        operands: vec![
                            Operand::Register(0),
                            Operand::Register(1),
                            Operand::ConstIndex(1),
                        ],
                    },
                    Instruction {
                        pc: 2,
                        op: Op::StoreProperty,
                        operands: vec![
                            Operand::Register(1),
                            Operand::ConstIndex(0),
                            Operand::Register(2),
                            Operand::Register(3),
                        ],
                    },
                    Instruction {
                        pc: 3,
                        op: Op::ReturnUndefined,
                        operands: Vec::new(),
                    },
                ]
                .into(),
                ..Function::default()
            }],
            constants: vec![
                Constant::String {
                    utf16: "x".encode_utf16().collect(),
                },
                Constant::String {
                    utf16: "y".encode_utf16().collect(),
                },
            ],
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        }
    }

    fn wide_method_module() -> BytecodeModule {
        BytecodeModule {
            module: "runtime-value-method-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![
                Function {
                    id: 0,
                    name: "wideMethodBoundary".to_string(),
                    locals: 7,
                    code: vec![
                        Instruction {
                            pc: 0,
                            op: Op::CallMethodValue,
                            operands: vec![
                                Operand::Register(0),
                                Operand::Register(1),
                                Operand::ConstIndex(0),
                                Operand::ConstIndex(5),
                                Operand::Register(2),
                                Operand::Register(3),
                                Operand::Register(4),
                                Operand::Register(5),
                                Operand::Register(6),
                            ],
                        },
                        Instruction {
                            pc: 1,
                            op: Op::ReturnUndefined,
                            operands: Vec::new(),
                        },
                    ]
                    .into(),
                    ..Function::default()
                },
                Function {
                    id: 1,
                    name: "methodTarget".to_string(),
                    code: vec![Instruction {
                        pc: 0,
                        op: Op::ReturnUndefined,
                        operands: Vec::new(),
                    }]
                    .into(),
                    ..Function::default()
                },
            ],
            constants: (0..=5)
                .map(|index| Constant::String {
                    utf16: if index == 0 { "run" } else { "unused" }
                        .encode_utf16()
                        .collect(),
                })
                .collect(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        }
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

    #[test]
    fn stack_owned_named_properties_decode_and_validate_published_pc() {
        let mut vm = Interpreter::new();
        let context = vm
            .link_module(named_property_module())
            .expect("valid bytecode fixture");
        vm.ensure_property_ic_capacity(&context);
        let mut receiver = vm
            .allocate_object_literal_value()
            .expect("ordinary receiver");
        let mut setup_roots = otter_gc::RootScope::new(&mut vm.gc_heap);
        // SAFETY: `receiver` remains live until `setup_roots` is dropped after
        // both potentially allocating fixture stores.
        unsafe {
            setup_roots.add_value(&mut receiver);
        }
        let object = receiver.as_object().expect("ordinary object");
        vm.set_property(object, "x", Value::number_i32(11))
            .expect("x fixture");
        let object = receiver.as_object().expect("relocated ordinary object");
        vm.set_property(object, "y", Value::number_i32(22))
            .expect("y fixture");
        drop(setup_roots);
        let mut stack = ActivationStack::new();
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [
            Value::undefined(),
            receiver,
            Value::number_i32(33),
            Value::undefined(),
        ];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 1,
                register_count: 4,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();

        // SAFETY: activation, frame, and register window remain live and
        // exclusively owned for this boundary scope.
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("stack-owned runtime call");
        let (y, _) = call.load_property_value(receiver).expect("pc 1 selects y");
        assert_eq!(y.as_i32(), Some(22));

        call.set_pc(0);
        let (x, _) = call.load_property_value(receiver).expect("pc 0 selects x");
        assert_eq!(x.as_i32(), Some(11));
        let (_, fill) = call.load_property_value(receiver).expect("warmed x load");
        assert!(fill.is_some(), "warmed own-data load should seed the cell");

        call.set_pc(2);
        call.store_property_value(receiver, Value::number_i32(33))
            .expect("pc 2 selects x store");
        assert!(
            call.store_property_value(receiver, Value::number_i32(33))
                .expect("warmed x store")
                .is_some(),
            "ordinary existing-slot store should seed the cell"
        );
        call.set_pc(0);
        assert_eq!(
            call.load_property_value(receiver)
                .expect("updated x")
                .0
                .as_i32(),
            Some(33)
        );

        call.set_pc(3);
        assert!(matches!(
            call.load_property_value(receiver),
            Err(VmError::InvalidOperand)
        ));
    }

    #[test]
    fn stack_owned_wide_method_span_decodes_count_and_records_exact_target() {
        let mut vm = Interpreter::new();
        let context = vm
            .link_module(wide_method_module())
            .expect("valid bytecode fixture");
        vm.ensure_property_ic_capacity(&context);
        let mut receiver = vm
            .allocate_object_literal_value()
            .expect("ordinary method receiver");
        {
            let mut setup_roots = otter_gc::RootScope::new(&mut vm.gc_heap);
            // SAFETY: `receiver` outlives the scope and is re-read after the
            // potentially allocating hidden-class transition.
            unsafe {
                setup_roots.add_value(&mut receiver);
            }
            vm.set_property(
                receiver.as_object().expect("method receiver object"),
                "run",
                Value::function(1),
            )
            .expect("install bytecode method");
        }

        let mut stack = ActivationStack::new();
        let mut registers = [
            Value::undefined(),
            receiver,
            Value::number_i32(10),
            Value::number_i32(20),
            Value::number_i32(30),
            Value::number_i32(40),
            Value::number_i32(50),
        ];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 0,
                register_count: 7,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();
        let packet = [
            registers[1],
            registers[2],
            registers[3],
            registers[4],
            registers[5],
            registers[6],
        ];
        vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            let mut activation = VmRuntimeActivation::new(vm, stack, &context, 0);
            // SAFETY: activation/frame/register storage and the copied packet
            // remain live and exclusively owned for this boundary scope. The
            // exact activation stack is published by this runtime turn.
            let mut call = unsafe {
                RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame))
            }
            .expect("stack-owned method runtime call");
            assert!(
                call.call_method_values(&packet)
                    .expect("wide method completion")
                    .is_undefined()
            );
            assert!(matches!(
                call.call_method_values(&packet[..5]),
                Err(VmError::InvalidOperand)
            ));
            call.set_pc(1);
            assert!(matches!(
                call.call_method_values(&packet),
                Err(VmError::InvalidOperand)
            ));
        });

        let feedback_site = context
            .property_ic_site(0, 0)
            .expect("method feedback site");
        assert!(matches!(
            vm.method_target_feedback(feedback_site),
            Some(crate::MethodCallFeedback::Mono { method_fid: 1, .. })
        ));
    }

    #[test]
    fn named_store_boundary_publishes_canonical_inline_add_transition() {
        let mut vm = Interpreter::new();
        let context = vm
            .link_module(named_property_module())
            .expect("valid bytecode fixture");
        vm.ensure_property_ic_capacity(&context);
        let mut first = vm
            .allocate_object_literal_value()
            .expect("first ordinary receiver");
        let mut second = {
            let mut allocation_roots = otter_gc::RootScope::new(&mut vm.gc_heap);
            // SAFETY: `first` outlives the scope and is re-read only after the
            // potentially moving second allocation has rewritten this slot.
            unsafe {
                allocation_roots.add_value(&mut first);
            }
            vm.allocate_object_literal_value()
                .expect("second ordinary receiver")
        };
        {
            let mut setup_roots = otter_gc::RootScope::new(&mut vm.gc_heap);
            // SAFETY: both locals outlive the scope and are re-read after each
            // potentially moving shape allocation.
            unsafe {
                setup_roots.add_value(&mut first);
                setup_roots.add_value(&mut second);
            }
            assert!(
                vm.ordinary_set_data_property(
                    first.as_object().expect("first object"),
                    "anchor",
                    Value::number_i32(1),
                )
                .expect("first anchor")
            );
            assert!(
                vm.ordinary_set_data_property(
                    second.as_object().expect("second object"),
                    "anchor",
                    Value::number_i32(2),
                )
                .expect("second anchor")
            );
            // The fixture targets the null-prototype transition program so the
            // property key need not already be interned on `%Object.prototype%`.
            crate::object::set_prototype(
                first.as_object().expect("first object"),
                vm.gc_heap_mut(),
                None,
            );
            crate::object::set_prototype(
                second.as_object().expect("second object"),
                vm.gc_heap_mut(),
                None,
            );
        }
        let parent_shape =
            crate::object::shape(first.as_object().expect("first object"), vm.gc_heap()).offset();
        assert_eq!(
            crate::object::shape(second.as_object().expect("second object"), vm.gc_heap()).offset(),
            parent_shape,
            "fresh peers must share the transition parent"
        );

        let mut stack = ActivationStack::new();
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [first, second, Value::undefined(), Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 2,
                register_count: 4,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();
        // SAFETY: activation, frame, and both rooted locals remain live and
        // exclusively owned for the complete boundary scope.
        {
            let mut call = unsafe {
                RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame))
            }
            .expect("stack-owned runtime call");
            let first_way = call
                .store_property_value(registers[0], Value::number_i32(11))
                .expect("first canonical transition")
                .expect("inline transition way");
            assert!(first_way.is_add_transition());
            assert_eq!(first_way.receiver_shape, parent_shape);
            assert_ne!(first_way.transition_shape, 0);
            assert_eq!(
                first_way.value_byte,
                std::mem::size_of::<Value>() as u32,
                "the transition appends after the shared anchor slot"
            );

            let second_way = call
                .store_property_value(registers[1], Value::number_i32(22))
                .expect("installed VM transition replay")
                .expect("replayed transition way");
            assert_eq!(second_way, first_way);
        }
        assert_eq!(
            crate::object::get_own(
                registers[1].as_object().expect("second object"),
                vm.gc_heap(),
                "x",
            )
            .and_then(Value::as_i32),
            Some(22)
        );
    }

    #[test]
    fn named_store_default_prototype_keeps_cell_rhs_on_canonical_boundary() {
        let mut vm = Interpreter::new();
        let context = vm
            .link_module(named_property_module())
            .expect("valid bytecode fixture");
        vm.ensure_property_ic_capacity(&context);
        let mut first = Value::undefined();
        let mut second = Value::undefined();
        let mut first_rhs = Value::undefined();
        let mut second_rhs = Value::undefined();
        {
            let mut setup_roots = otter_gc::RootScope::new(&mut vm.gc_heap);
            // SAFETY: all four locals outlive the scope and are re-read only
            // after every potentially moving object/shape allocation rewrites
            // their exact slots.
            unsafe {
                setup_roots.add_value(&mut first);
                setup_roots.add_value(&mut second);
                setup_roots.add_value(&mut first_rhs);
                setup_roots.add_value(&mut second_rhs);
            }
            first = vm
                .allocate_object_literal_value()
                .expect("first ordinary peer");
            second = vm
                .allocate_object_literal_value()
                .expect("second ordinary peer");
            first_rhs = vm.allocate_object_literal_value().expect("first cell RHS");
            second_rhs = vm.allocate_object_literal_value().expect("second cell RHS");
            assert!(
                vm.ordinary_set_data_property(
                    first.as_object().expect("first ordinary peer"),
                    "anchor",
                    Value::number_i32(1),
                )
                .expect("first peer anchor")
            );
            assert!(
                vm.ordinary_set_data_property(
                    second.as_object().expect("second ordinary peer"),
                    "anchor",
                    Value::number_i32(2),
                )
                .expect("second peer anchor")
            );
            assert!(
                vm.ordinary_set_data_property(
                    first_rhs.as_object().expect("first cell RHS object"),
                    "marker",
                    Value::number_i32(41),
                )
                .expect("first RHS marker")
            );
            assert!(
                vm.ordinary_set_data_property(
                    second_rhs.as_object().expect("second cell RHS object"),
                    "marker",
                    Value::number_i32(42),
                )
                .expect("second RHS marker")
            );
        }

        let first_object = first.as_object().expect("first peer object");
        let second_object = second.as_object().expect("second peer object");
        let parent_shape = crate::object::shape(first_object, vm.gc_heap()).offset();
        assert_eq!(
            crate::object::shape(second_object, vm.gc_heap()).offset(),
            parent_shape,
            "fresh peers must share the default-prototype parent shape"
        );
        let first_prototype = crate::object::prototype(first_object, vm.gc_heap())
            .expect("ordinary object must use Object.prototype");
        assert_eq!(
            crate::object::prototype(second_object, vm.gc_heap()),
            Some(first_prototype),
            "both peers must retain the realm's ordinary Object.prototype"
        );
        assert!(
            crate::object::prototype(first_prototype, vm.gc_heap()).is_none(),
            "Object.prototype must be the terminal direct prototype"
        );

        let mut stack = ActivationStack::new();
        let mut registers = [first, second, first_rhs, second_rhs];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 2,
                register_count: 4,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();
        // SAFETY: `frame` and its register window remain stable and live until
        // the matching pop after the complete RuntimeCall scope.
        unsafe {
            vm.jit_push_native_frame(&mut frame)
                .expect("publish stack-owned test frame");
        }
        {
            let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
            // SAFETY: the activation, published frame, and register array are
            // exclusively owned for both transition calls.
            let mut call = unsafe {
                RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame))
            }
            .expect("stack-owned transition call");
            let first_way = call
                .store_property_value(registers[0], registers[2])
                .expect("first canonical Cell store");
            assert_eq!(
                first_way, None,
                "dictionary-shaped Object.prototype has no complete native absence guard"
            );

            let second_way = call
                .store_property_value(registers[1], registers[3])
                .expect("second canonical Cell store");
            assert_eq!(
                second_way, None,
                "each default-prototype peer remains on the canonical store boundary"
            );
        }
        vm.jit_pop_native_activation();

        for (receiver, rhs, marker) in [
            (registers[0], registers[2], 41),
            (registers[1], registers[3], 42),
        ] {
            let stored = crate::object::get_own(
                receiver.as_object().expect("relocated peer object"),
                vm.gc_heap(),
                "x",
            )
            .expect("transition must install x");
            assert_eq!(stored, rhs, "transition must preserve exact Cell identity");
            assert_eq!(
                crate::object::get_own(
                    stored.as_object().expect("stored Cell RHS"),
                    vm.gc_heap(),
                    "marker",
                )
                .and_then(Value::as_i32),
                Some(marker),
                "stored Cell payload must survive both transition paths"
            );
        }
    }

    #[test]
    fn named_store_transition_encodes_direct_terminal_prototype_guard() {
        let mut vm = Interpreter::new();
        let context = vm
            .link_module(named_property_module())
            .expect("valid bytecode fixture");
        vm.ensure_property_ic_capacity(&context);
        let mut prototype = Value::object(
            vm.alloc_runtime_rooted_object_with_roots(&[], &[])
                .expect("clean terminal prototype"),
        );
        let mut first = Value::undefined();
        let mut second = Value::undefined();
        {
            let mut allocation_roots = otter_gc::RootScope::new(&mut vm.gc_heap);
            // SAFETY: all three slots outlive the scope and are the sole
            // authorities re-read after each potentially moving allocation.
            unsafe {
                allocation_roots.add_value(&mut prototype);
                allocation_roots.add_value(&mut first);
                allocation_roots.add_value(&mut second);
            }
            first = vm
                .allocate_object_literal_value()
                .expect("first direct-prototype receiver");
            second = vm
                .allocate_object_literal_value()
                .expect("second direct-prototype receiver");
            let prototype = prototype.as_object().expect("prototype object");
            crate::object::set_prototype(
                first.as_object().expect("first object"),
                vm.gc_heap_mut(),
                Some(prototype),
            );
            crate::object::set_prototype(
                second.as_object().expect("second object"),
                vm.gc_heap_mut(),
                Some(prototype),
            );
        }
        let prototype_shape = crate::object::shape(
            prototype.as_object().expect("prototype object"),
            vm.gc_heap(),
        )
        .offset();
        assert_ne!(prototype_shape, 0);

        let mut stack = ActivationStack::new();
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [prototype, first, second, Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 2,
                register_count: 4,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();
        // SAFETY: activation/frame/register storage outlive the boundary.
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("stack-owned runtime call");
        let first_way = call
            .store_property_value(registers[1], Value::number_i32(7))
            .expect("first direct-prototype transition")
            .expect("direct-prototype transition way");
        assert!(first_way.is_add_transition());
        assert_eq!(first_way.holder_shape, prototype_shape);
        let second_way = call
            .store_property_value(registers[2], Value::number_i32(9))
            .expect("transition replay")
            .expect("replayed direct-prototype way");
        assert_eq!(second_way, first_way);
    }

    #[test]
    fn strict_named_store_boundary_rejects_non_extensible_receiver() {
        let mut vm = Interpreter::new();
        let mut module = named_property_module();
        module.functions[0].is_strict = true;
        let context = vm.link_module(module).expect("valid bytecode fixture");
        vm.ensure_property_ic_capacity(&context);
        let receiver = vm
            .allocate_object_literal_value()
            .expect("ordinary receiver");
        crate::object::prevent_extensions(
            receiver.as_object().expect("receiver object"),
            vm.gc_heap_mut(),
        );

        let mut stack = ActivationStack::new();
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [
            receiver,
            Value::undefined(),
            Value::undefined(),
            Value::undefined(),
        ];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 2,
                register_count: 4,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();
        // SAFETY: activation/frame/register storage outlive the boundary.
        {
            let mut call = unsafe {
                RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame))
            }
            .expect("stack-owned runtime call");
            assert!(
                call.store_property_value(registers[0], Value::number_i32(1))
                    .is_err(),
                "strict StoreProperty must throw instead of using construction-time set"
            );
        }
        assert_eq!(
            crate::object::get_own(
                registers[0].as_object().expect("rooted receiver"),
                vm.gc_heap(),
                "x",
            ),
            None,
            "the rejected store must not mutate the receiver"
        );
    }
}
