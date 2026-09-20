//! Typed access to one compiled VM activation and its inlined descendants.
//!
//! # Contents
//! - [`RuntimeCall`] is the short-lived JIT-to-VM semantic boundary.
//! - `exceptions` resolves static catch-only handlers for stack-owned frames
//!   through the same CodeBlock metadata used by exact deoptimization.
//! - A private frame identity distinguishes an interpreter-owned root from a
//!   generated stack-owned activation.
//! - Focused `control` and `value_ops` implementations expose typed
//!   operations instead of raw interpreter, stack, context, or frame handles.
//! - `inline_activations` publishes boxed callee recipes only for committed
//!   cold reentry and returns to the same compiled body without interpretation.
//!
//! # Invariants
//! - Construction reads and validates scalar descriptors from the published
//!   runtime record and current [`NativeFrame`], resolves that function's
//!   owning chunk, and retains only owned context/`NonNull` service identities,
//!   never references to either owner container or a copied frame record.
//! - No method returns the interpreter, materialized stack, native frame,
//!   register pointer, or an [`ActiveFrameMut`] view. Frame views exist only
//!   inside one typed method and never survive a VM transition.
//! - A semantic operation may open the VM/context reference required by the
//!   existing exclusive-mutator contract. Native-frame and stack slot views
//!   remain operation-scoped and are not retained across that call.
//! - A native frame's ownership flags are decoded once. The few genuinely
//!   materialized-only cold operations reject non-materialized frames before
//!   mutating state; total value-level operations never side-exit for
//!   representation.
//! - Register/upvalue windows stay published across allocating operations;
//!   slot access remains checked and scoped through [`ActiveFrameMut`].
//! - Binding allocates no wrapper, lock, side table, or thread-local state.
//!   Scoped inline reentry owns temporary native records and captured spines;
//!   every such record is removed before its storage is released.
//!
//! # See also
//! - [`crate::jit::VmRuntimeActivation`] owns the opaque entry-lifetime record.
//! - [`crate::active_frame`] validates the machine-published frame windows.

mod bindings;
mod class_ops;
mod committed_values;
mod control;
mod exceptions;
mod forward_arguments;
mod inline_activations;
mod iterators;
mod value_loads;
mod value_ops;

pub use class_ops::ClassRuntimeOp;
pub use committed_values::{CommittedValueError, ObjectProtocolValueOp, ScalarValueOp};
pub use iterators::IteratorRuntimeOutcome;
pub use value_loads::ValueLoadRuntimeOp;

use std::{marker::PhantomData, ptr::NonNull};

use crate::{
    ActivationStack, ActiveFrameMut, ActiveFrameRef, ExecutionContext, Interpreter, Value, VmError,
    jit::VmRuntimeActivation,
    native_abi::{NativeFrame, NativeFrameFlags},
};

/// Physical ownership decoded from the native frame and runtime activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeFrameIdentity {
    /// Compiled view of an existing interpreter activation.
    Materialized(usize),
    /// Generated-code stack window rooted by the published native frame.
    StackOwned,
}

/// Exclusive, short-lived semantic view of one compiled activation.
///
/// This type owns the current function's immutable execution context plus
/// branded raw descriptors to mutable VM services. JIT stubs can request
/// operations, read/write one checked register, and inspect scalar frame
/// identity only. In particular, no retained native-frame or
/// materialized-stack reference aliases the GC's active-frame root walk.
pub struct RuntimeCall<'a> {
    pub(super) vm: NonNull<Interpreter>,
    pub(super) stack: NonNull<ActivationStack>,
    pub(super) context: ExecutionContext,
    pub(super) frame: NonNull<NativeFrame>,
    identity: RuntimeFrameIdentity,
    _exclusive: PhantomData<&'a mut ()>,
}

impl<'a> RuntimeCall<'a> {
    /// Bind the opaque entry-lifetime record to its currently published frame.
    ///
    /// VM-service pointer validation happens here once. Operation methods open
    /// short references only for the exact semantic call they perform.
    ///
    /// # Safety
    ///
    /// `activation` must be the live record created from exclusive VM/stack
    /// borrows for this entry. `frame` and all non-empty windows it describes
    /// must remain initialized, published, and exclusively mutator-owned for
    /// the returned call's lifetime.
    pub unsafe fn bind(
        activation: NonNull<VmRuntimeActivation>,
        frame: NonNull<NativeFrame>,
    ) -> Result<Self, VmError> {
        // SAFETY: the caller keeps the activation record live for `'a`. Copy
        // only its opaque pointers; no reference is retained across a VM call.
        let activation = unsafe { activation.as_ref() };
        let vm = NonNull::new(activation.vm).ok_or(VmError::InvalidOperand)?;
        let stack = NonNull::new(activation.stack).ok_or(VmError::InvalidOperand)?;
        let ambient = unsafe { activation.context.as_ref() }.ok_or(VmError::InvalidOperand)?;
        // Validate both published windows before exposing any semantic method.
        // SAFETY: the entry contract retains the initialized raw descriptor for
        // `'a`; ActiveFrameRef itself stores no native Rust reference.
        unsafe { ActiveFrameRef::from_native_ptr(frame.as_ptr()) }
            .map_err(|_| VmError::InvalidOperand)?;
        // Read only the scalars needed for ownership validation. Retaining a
        // copied frame would create a second, stale-looking carrier for its GC
        // slots even though no allocation occurs during this bind.
        // SAFETY: the validated frame remains initialized for `'a`.
        let (flags, function_id, register_count, register_base, upvalue_base, upvalue_count) = {
            // SAFETY: one operation-scoped shared view; every retained value is
            // scalar and the view ends before any semantic VM entry.
            let frame = unsafe { frame.as_ref() };
            (
                frame.header.flags,
                frame.header.function_id,
                frame.header.register_count,
                frame.register_base,
                frame.upvalue_base,
                frame.upvalue_count,
            )
        };
        let identity = if flags.contains(NativeFrameFlags::STACK_REGISTERS) {
            RuntimeFrameIdentity::StackOwned
        } else {
            let frame_index = activation.frame_index();
            // SAFETY: the opaque stack pointer was validated above and the
            // activation contract keeps the stack live for this bind.
            let materialized = unsafe { stack.as_ref() }
                .get(frame_index)
                .ok_or(VmError::InvalidOperand)?;
            let expected_upvalue_base = if materialized.upvalues.is_empty() {
                0
            } else {
                materialized.upvalues.as_ptr() as u64
            };
            if materialized.function_id != function_id
                || materialized.registers.len() != usize::from(register_count)
                || materialized.registers.as_ptr() as u64 != register_base
                || expected_upvalue_base != upvalue_base
                || materialized.upvalues.len() != upvalue_count as usize
            {
                return Err(VmError::InvalidOperand);
            }
            RuntimeFrameIdentity::Materialized(frame_index)
        };
        let context = ambient
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let context = (*context).clone();
        Ok(Self {
            vm,
            stack,
            context,
            frame,
            identity,
            _exclusive: PhantomData,
        })
    }

    /// Current physical owner for boundary tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn identity(&self) -> RuntimeFrameIdentity {
        self.identity
    }

    /// Read one checked register and end the frame borrow before returning.
    pub fn read(&self, register: u16) -> Result<Value, VmError> {
        // SAFETY: construction validated the published frame and its windows.
        unsafe { ActiveFrameRef::from_native_ptr(self.frame.as_ptr()) }
            .map_err(|_| VmError::InvalidOperand)?
            .read(register)
    }

    /// Write one checked register and end the frame borrow before returning.
    pub fn write(&mut self, register: u16, value: Value) -> Result<(), VmError> {
        self.with_frame(|frame| frame.write(register, value))
    }

    /// Current function identity.
    #[must_use]
    pub fn function_id(&self) -> u32 {
        // SAFETY: one scalar copy from the live published descriptor.
        unsafe { self.frame.as_ref().header.function_id }
    }

    /// Current logical instruction index.
    #[must_use]
    pub fn pc(&self) -> u32 {
        // SAFETY: one scalar copy from the live published descriptor.
        unsafe { self.frame.as_ref().header.pc }
    }

    /// Publish a logical resume instruction.
    pub fn set_pc(&mut self, pc: u32) {
        // SAFETY: exclusive logical ownership is branded by `&mut self`.
        unsafe { self.frame.as_mut().header.pc = pc };
    }

    /// Complete one decoded control-family operation against the published
    /// native activation.
    pub fn control_op(
        &mut self,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = self.context.clone();
        self.with_frame(|frame| {
            vm.jit_runtime_control_op(&context, frame, opcode, arg0, arg1, arg2)
        })
    }

    /// Complete one materialized global-access transition while the native
    /// descriptor remains the sole owner of representation-neutral frame
    /// state such as the direct-eval environment.
    pub fn global_op(
        &mut self,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        let RuntimeFrameIdentity::Materialized(frame_index) = self.identity else {
            return Err(VmError::InvalidOperand);
        };
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = self.context.clone();
        self.with_frame(|frame| {
            vm.jit_runtime_global_op(
                &context,
                stack,
                frame_index,
                frame,
                opcode,
                arg0,
                arg1,
                arg2,
            )
        })
    }

    /// Complete one materialized `delete` transition against the published
    /// native activation. Dynamic-name resolution reads the native frame's
    /// sole eval-environment root; property/element drivers retain the
    /// canonical materialized stack path.
    pub fn delete_op(
        &mut self,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        let RuntimeFrameIdentity::Materialized(frame_index) = self.identity else {
            return Err(VmError::InvalidOperand);
        };
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = self.context.clone();
        self.with_frame(|frame| {
            vm.jit_runtime_delete_op(
                &context,
                stack,
                frame_index,
                frame,
                opcode,
                arg0,
                arg1,
                arg2,
            )
        })
    }

    /// Run one synchronous direct eval with the materialized frame as the sole
    /// eval-environment root for the duration of interpreter reentry, then
    /// return ownership to the published native descriptor on every outcome.
    pub fn eval_op(&mut self, packed_registers: u64, flags: u64, site: u64) -> Result<(), VmError> {
        let RuntimeFrameIdentity::Materialized(frame_index) = self.identity else {
            return Err(VmError::InvalidOperand);
        };
        let native = self.frame.as_ptr();
        let stack = unsafe { &mut *self.stack.as_ptr() };
        {
            let materialized = stack.get_mut(frame_index).ok_or(VmError::InvalidOperand)?;
            // SAFETY: RuntimeCall exclusively owns the published native
            // descriptor. The short reference ends before VM reentry.
            let native_frame = unsafe { &mut *native };
            if materialized.function_id != native_frame.header.function_id
                || !materialized.eval_env.is_null()
            {
                return Err(VmError::InvalidOperand);
            }
            std::mem::swap(&mut materialized.eval_env, &mut native_frame.eval_env);
        }

        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = &self.context;
        let result =
            vm.jit_runtime_eval_op(context, stack, frame_index, packed_registers, flags, site);

        let materialized = stack.get_mut(frame_index).ok_or(VmError::InvalidOperand)?;
        // SAFETY: the native descriptor stayed published and exclusively
        // owned. Direct eval may move or replace the materialized slot, so it
        // is deliberately re-fetched only after the VM call completes.
        let native_frame = unsafe { &mut *native };
        let invalid_owner = materialized.function_id != native_frame.header.function_id
            || !native_frame.eval_env.is_null();
        std::mem::swap(&mut materialized.eval_env, &mut native_frame.eval_env);
        if invalid_owner {
            return Err(VmError::InvalidOperand);
        }
        result
    }

    pub(super) fn with_frame<T>(
        &mut self,
        operation: impl FnOnce(&mut ActiveFrameMut<'_>) -> Result<T, VmError>,
    ) -> Result<T, VmError> {
        // SAFETY: construction validated the frame and owns it exclusively.
        let mut frame = unsafe { ActiveFrameMut::from_native_ptr(self.frame.as_ptr()) }
            .map_err(|_| VmError::InvalidOperand)?;
        operation(&mut frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::{NativeFrameKind, VmFrameHeader};
    use otter_bytecode::{Function, Instruction, Op, Operand, SourceKind};

    fn bind_this_fixture() -> (ExecutionContext, Function) {
        let function = Function {
            id: 0,
            name: "derived".to_string(),
            locals: 1,
            is_derived_constructor: true,
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::BindThisValue,
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 1,
                    op: Op::ReturnUndefined,
                    operands: Vec::new(),
                },
            ]
            .into(),
            ..Function::default()
        };
        let context = ExecutionContext::from_module(crate::BytecodeModule {
            module: "committed-bind-this-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![function.clone()],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        })
        .expect("valid bytecode fixture");
        (context, function)
    }

    #[test]
    fn identity_is_decoded_once_and_slot_access_is_checked() {
        let mut vm = Interpreter::new();
        let mut stack = ActivationStack::new();
        let context = ExecutionContext::from_module(crate::test_support::minimal_bytecode_module(
            "runtime-call-test.js",
        ))
        .expect("valid bytecode fixture");
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [Value::number_i32(3), Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 11,
                register_count: 2,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();

        // SAFETY: the local activation, frame, and register array stay live and
        // exclusively owned for the RuntimeCall scope.
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .unwrap();
        assert_eq!(call.identity(), RuntimeFrameIdentity::StackOwned);
        assert_eq!(call.read(0).unwrap(), Value::number_i32(3));
        call.write(1, Value::boolean(true)).unwrap();
        assert!(matches!(call.read(2), Err(VmError::InvalidOperand)));
        assert_eq!(registers[1], Value::boolean(true));
    }

    #[test]
    fn materialized_identity_must_resolve_the_published_stack_slot() {
        let mut vm = Interpreter::new();
        let mut stack = ActivationStack::new();
        let context = ExecutionContext::from_module(crate::test_support::minimal_bytecode_module(
            "runtime-call-materialized-test.js",
        ))
        .expect("valid bytecode fixture");
        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);

        assert!(matches!(
            // SAFETY: pointers are live; the test deliberately supplies a
            // materialized identity that cannot resolve in the empty stack.
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) },
            Err(VmError::InvalidOperand)
        ));
    }

    #[test]
    fn runtime_activation_moves_one_eval_env_owner_for_every_native_outcome() {
        let (context, function) = bind_this_fixture();
        let mut vm = Interpreter::new();
        let before = crate::eval_env::alloc_eval_env(&mut vm.gc_heap, None).expect("eval env");
        let after = crate::eval_env::alloc_eval_env(&mut vm.gc_heap, None).expect("moved eval env");
        let mut materialized = vm
            .test_frame_for_function(&function)
            .expect("materialized frame");
        materialized.eval_env = before;
        let cold = vm.frame_ensure_cold(&mut materialized);
        cold.new_target = Some(Value::function(99));
        cold.is_derived_constructor = true;
        let mut stack = ActivationStack::new();
        stack.push(materialized);

        let activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [Value::undefined()];
        let mut native = NativeFrame::new(
            VmFrameHeader {
                function_id: function.id,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(function.id),
            Value::undefined(),
        );
        activation
            .initialize_native_frame_state(&mut native)
            .expect("non-lexical state mirror");

        assert!(native.eval_env().is_none());
        assert_eq!(native.new_target(), Value::function(99));
        assert!(native.is_derived_constructor());
        for outcome in [0_u8, 1, 2, 3] {
            stack[0].eval_env = before;
            let returned = unsafe {
                activation.with_native_eval_env_owner(std::ptr::addr_of_mut!(native), || {
                    assert!(stack[0].eval_env.is_null());
                    assert_eq!(native.eval_env(), Some(before));
                    native.set_eval_env(Some(after));
                    outcome
                })
            }
            .expect("eval env ownership transaction");
            assert_eq!(returned, outcome);
            assert_eq!(stack[0].eval_env, after);
            assert!(native.eval_env().is_none());
        }
        assert_eq!(std::mem::size_of_val(&activation), 32);
    }

    #[test]
    fn committed_throw_preserves_nested_frames_until_local_catch_acknowledges_it() {
        let mut vm = Interpreter::new();
        let exception = Value::number_i32(73);
        vm.set_pending_uncaught_throw(exception);
        let nested_frames = vec![crate::StackFrameSnapshot {
            function_id: 91,
            function_name: "nestedGetter".to_string(),
            module: "runtime-call-throw-test.js".to_string(),
            span: (11, 19),
        }];
        vm.pending_uncaught_frames = Some(nested_frames.clone());
        let _ = vm.err_uncaught("stale committed detail".into());

        let mut stack = ActivationStack::new();
        let context = ExecutionContext::from_module(crate::test_support::minimal_bytecode_module(
            "runtime-call-throw-test.js",
        ))
        .expect("valid bytecode fixture");
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();

        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("runtime call");
        assert_eq!(call.take_js_throw(VmError::Uncaught), Ok(exception));
        assert!(vm.pending_uncaught_throw.is_none());
        assert_eq!(
            vm.pending_uncaught_frames.as_deref(),
            Some(nested_frames.as_slice())
        );
        assert!(vm.error_detail().is_none());

        vm.jit_acknowledge_caught_throw();
        assert!(vm.pending_uncaught_frames.is_none());

        let later_exception = Value::number_i32(99);
        let later_frames = vec![crate::StackFrameSnapshot {
            function_id: 92,
            function_name: "catchBody".to_string(),
            module: "runtime-call-throw-test.js".to_string(),
            span: (23, 29),
        }];
        vm.set_pending_uncaught_throw(later_exception);
        vm.pending_uncaught_frames = Some(later_frames.clone());
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("runtime call after catch acknowledgement");
        assert_eq!(call.take_js_throw(VmError::Uncaught), Ok(later_exception));
        assert_eq!(
            vm.pending_uncaught_frames.as_deref(),
            Some(later_frames.as_slice())
        );
        assert_ne!(
            vm.pending_uncaught_frames.as_deref(),
            Some(nested_frames.as_slice())
        );
    }

    #[test]
    fn wrong_committed_site_is_fatal_before_semantic_entry() {
        let mut vm = Interpreter::new();
        let before = vm.jit_runtime_stats();
        let mut stack = ActivationStack::new();
        let context = ExecutionContext::from_module(crate::test_support::minimal_bytecode_module(
            "runtime-call-wrong-site-test.js",
        ))
        .expect("valid bytecode fixture");
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 37,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();

        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("runtime call");
        assert!(matches!(
            call.object_protocol_values(Value::undefined(), Value::undefined()),
            Err(CommittedValueError::Fatal(VmError::InvalidOperand))
        ));
        assert_eq!(vm.jit_runtime_stats(), before);
        assert!(vm.pending_uncaught_throw.is_none());
        assert!(vm.pending_uncaught_frames.is_none());
        assert!(vm.error_detail().is_none());
    }

    #[test]
    fn committed_materialized_bind_this_never_advances_the_published_pc() {
        let (context, function) = bind_this_fixture();
        let mut vm = Interpreter::new();
        let mut materialized = vm
            .test_frame_for_function(&function)
            .expect("materialized frame");
        materialized.this_value = Value::hole();
        vm.frame_ensure_cold(&mut materialized)
            .is_derived_constructor = true;
        let register_base = materialized.registers.as_mut_ptr() as u64;
        let mut native = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            register_base,
            Value::function(0),
            Value::hole(),
        );
        native.set_derived_constructor();
        let mut stack = ActivationStack::new();
        stack.push(materialized);
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let bound = Value::number_i32(41);

        let mut call = unsafe {
            RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut native))
        }
        .expect("runtime call");
        assert_eq!(
            call.scalar_values(bound, Value::undefined())
                .expect("committed bind"),
            bound
        );
        assert_eq!(native.header.pc, 0);
        assert_eq!(stack[0].pc, 0);
        assert_eq!(native.this_value(), bound);
        assert_eq!(stack[0].this_value, bound);

        assert!(matches!(
            call.scalar_values(Value::number_i32(42), Value::undefined()),
            Err(CommittedValueError::JavaScript(VmError::ThisUninitialized))
        ));
        assert_eq!(native.header.pc, 0);
        assert_eq!(stack[0].pc, 0);
        assert_eq!(native.this_value(), bound);
        assert_eq!(stack[0].this_value, bound);
    }

    #[test]
    fn committed_materialized_lexical_bind_targets_the_outer_derived_frame() {
        let (base_context, outer_function) = bind_this_fixture();
        let inner_function = Function {
            id: 1,
            name: "nested_arrow".to_string(),
            locals: 1,
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::BindThisValue,
                    operands: vec![Operand::Register(0)],
                },
                Instruction {
                    pc: 1,
                    op: Op::ReturnUndefined,
                    operands: Vec::new(),
                },
            ]
            .into(),
            ..Function::default()
        };
        let context = ExecutionContext::from_module(crate::BytecodeModule {
            module: "committed-lexical-bind-this-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![outer_function.clone(), inner_function.clone()],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        })
        .expect("valid bytecode fixture");
        drop(base_context);

        let mut vm = Interpreter::new();
        let mut outer = vm
            .test_frame_for_function(&outer_function)
            .expect("outer derived frame");
        outer.this_value = Value::hole();
        vm.frame_ensure_cold(&mut outer).is_derived_constructor = true;
        let mut inner = vm
            .test_frame_for_function(&inner_function)
            .expect("nested lexical frame");
        inner.this_value = Value::hole();
        let inner_register_base = inner.registers.as_mut_ptr() as u64;

        let mut native = NativeFrame::new(
            VmFrameHeader {
                function_id: 1,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            inner_register_base,
            Value::function(1),
            Value::hole(),
        );
        let mut stack = ActivationStack::new();
        stack.push(outer);
        stack.push(inner);
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 1);
        let bound = Value::number_i32(71);

        let mut call = unsafe {
            RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut native))
        }
        .expect("nested runtime call");
        assert_eq!(call.identity(), RuntimeFrameIdentity::Materialized(1));
        assert_eq!(
            call.scalar_values(bound, Value::undefined())
                .expect("lexical committed bind"),
            bound
        );
        assert_eq!(native.header.pc, 0);
        assert_eq!(stack[0].this_value, bound);
        assert!(stack[1].this_value.is_hole());
        assert_eq!(native.this_value(), bound);

        assert!(matches!(
            call.scalar_values(Value::number_i32(72), Value::undefined()),
            Err(CommittedValueError::JavaScript(VmError::ThisUninitialized))
        ));
        assert_eq!(stack[0].this_value, bound);
        assert_eq!(native.this_value(), bound);
    }

    #[test]
    fn wrong_frame_bind_this_is_fatal_before_a_local_handler_or_effect() {
        let (context, _) = bind_this_fixture();
        let mut vm = Interpreter::new();
        let before = vm.jit_runtime_stats();
        let mut stack = ActivationStack::new();
        let mut registers = [Value::undefined()];
        let mut native = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::hole(),
        );
        native.set_stack_registers();
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);

        let mut call = unsafe {
            RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut native))
        }
        .expect("runtime call");
        assert!(matches!(
            call.scalar_values(Value::number_i32(9), Value::undefined()),
            Err(CommittedValueError::Fatal(VmError::InvalidOperand))
        ));
        assert_eq!(native.header.pc, 0);
        assert!(native.this_value().is_hole());
        assert_eq!(vm.jit_runtime_stats(), before);
        assert!(vm.error_detail().is_none());
    }
}
