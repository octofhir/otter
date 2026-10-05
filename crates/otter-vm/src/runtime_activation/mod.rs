//! Typed semantic access to the currently published JavaScript activation.
//!
//! # Contents
//! - [`RuntimeCall`] binds VM services, function context and one native frame.
//! - Focused modules implement bindings, values, classes, calls and exceptions.
//! - Code-owned inline recipes identify sources for committed cold reentry.
//!
//! # Invariants
//! Binding validates the register and actual-argument descriptors and resolves
//! the function's owning context. Frame views stay local to one operation;
//! no slot borrow survives allocation or JavaScript reentry. Every tier uses
//! the same physical frame identity and caller chain. Logical inline sources
//! remain recipes until an exact deoptimization creates their activations.
//!
//! # See also
//! - [`crate::jit::VmRuntimeActivation`] for entry-lifetime service pointers.
//! - [`crate::active_frame`] for checked, short-lived window access.
//! - [`crate::native_abi::call_trampoline`] for call ownership.

mod bindings;
mod class_ops;
mod committed_values;
mod control;
mod deopt;
mod exceptions;
mod forward_arguments;
mod iterators;
pub(crate) mod semantic_source;
mod value_loads;
mod value_ops;

pub use class_ops::ClassRuntimeOp;
pub use committed_values::{
    BinaryOperator, CommittedValueError, ObjectProtocolValueOp, ScalarValueOp,
};
pub use control::BackedgePollOutcome;
pub use iterators::IteratorRuntimeOutcome;
pub use value_loads::ValueLoadRuntimeOp;

use std::{marker::PhantomData, ptr::NonNull};

use crate::{
    ActivationStack, ActiveFrameMut, ActiveFrameRef, ExecutionContext, Interpreter, Value, VmError,
    jit::VmRuntimeActivation, native_abi::Frame,
};

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
    pub(super) frame: NonNull<Frame>,
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
        frame: NonNull<Frame>,
    ) -> Result<Self, VmError> {
        // SAFETY: the caller keeps the activation record live for `'a`. Copy
        // only its opaque pointers; no reference is retained across a VM call.
        let activation = unsafe { activation.as_ref() };
        let vm = NonNull::new(activation.vm).ok_or(VmError::InvalidOperand)?;
        let stack = NonNull::new(activation.stack).ok_or(VmError::InvalidOperand)?;

        // Validate both published windows before exposing any semantic method.
        // SAFETY: the entry contract retains the initialized raw descriptor for
        // `'a`; ActiveFrameRef itself stores no native Rust reference.
        unsafe { ActiveFrameRef::from_ptr(frame.as_ptr()) }.map_err(|_| VmError::InvalidOperand)?;
        let function_id = unsafe { frame.as_ref().header.function_id };
        // A host-only entry may have no admitted source. Once a bytecode
        // frame is published, its own live FunctionID is the authoritative
        // metadata owner, including transparent Proxy/Bound child entries.
        let context =
            unsafe { activation.owner_context(function_id) }.ok_or(VmError::InvalidOperand)?;
        Ok(Self {
            vm,
            stack,
            context,
            frame,
            _exclusive: PhantomData,
        })
    }

    /// Absolute index in the one physical activation chain.
    fn frame_index(&self) -> Result<usize, VmError> {
        usize::try_from(unsafe { self.frame.as_ref().depth })
            .ok()
            .and_then(|depth| depth.checked_sub(1))
            .ok_or(VmError::InvalidOperand)
    }

    /// Read one checked register and end the frame borrow before returning.
    pub fn read(&self, register: u16) -> Result<Value, VmError> {
        // SAFETY: construction validated the published frame and its windows.
        unsafe { ActiveFrameRef::from_ptr(self.frame.as_ptr()) }
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

    /// Complete one materialized global-access transition against the
    /// published native activation.
    pub fn global_op(
        &mut self,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), CommittedValueError> {
        let frame_index = self
            .frame_index()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = self.context.clone();
        self.with_frame(|frame| {
            Ok(vm.jit_runtime_global_op(
                &context,
                stack,
                frame_index,
                frame,
                opcode,
                arg0,
                arg1,
                arg2,
            ))
        })
        .map_err(CommittedValueError::Fatal)?
    }

    /// Complete one materialized `delete` transition against the published
    /// native activation; property/element drivers retain the canonical
    /// materialized stack path.
    pub fn delete_op(
        &mut self,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), CommittedValueError> {
        let frame_index = self
            .frame_index()
            .map_err(|error| CommittedValueError::Fatal(error.into()))?;
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let context = self.context.clone();
        self.with_frame(|frame| {
            Ok(vm.jit_runtime_delete_op(
                &context,
                stack,
                frame_index,
                frame,
                opcode,
                arg0,
                arg1,
                arg2,
            ))
        })
        .map_err(CommittedValueError::Fatal)?
    }

    /// Run one synchronous direct eval from the materialized caller frame.
    /// `packed_registers` carries the `dst`, `src`, and `ctx` registers in
    /// 16-bit lanes; `flags` is the instruction's flags immediate.
    pub fn eval_op(&mut self, packed_registers: u64, flags: u64) -> Result<(), VmError> {
        let frame_index = self.frame_index()?;
        let stack = unsafe { &mut *self.stack.as_ptr() };
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = &self.context;
        vm.jit_runtime_eval_op(context, stack, frame_index, packed_registers, flags)
    }

    pub(super) fn with_frame<T>(
        &mut self,
        operation: impl FnOnce(&mut ActiveFrameMut<'_>) -> Result<T, VmError>,
    ) -> Result<T, VmError> {
        // SAFETY: construction validated the frame and owns it exclusively.
        let mut frame = unsafe { ActiveFrameMut::from_ptr(self.frame.as_ptr()) }
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
        let context = ExecutionContext::from_module(
            crate::BytecodeModule {
                module: "committed-bind-this-test.js".to_string(),
                template_sites: Vec::new(),
                source_kind: SourceKind::JavaScript,
                functions: vec![function.clone()],
                constants: Vec::new(),
                module_resolutions: Vec::new(),
                module_inits: Vec::new(),
                function_source: None,
            },
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture");
        (context, function)
    }

    #[test]
    fn identity_is_decoded_once_and_slot_access_is_checked() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut stack = crate::test_support::FrameChainFixture::new();
        let context = ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("runtime-call-test.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture");
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));
        let mut registers = [Value::number_i32(3), Value::undefined()];
        let mut frame = Frame::new(
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

        // SAFETY: the local activation, frame, and register array stay live and
        // exclusively owned for the RuntimeCall scope.
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .unwrap();
        assert_eq!(call.function_id(), frame.header.function_id);
        assert_eq!(call.read(0).unwrap(), Value::number_i32(3));
        call.write(1, Value::boolean(true)).unwrap();
        assert!(matches!(call.read(2), Err(VmError::InvalidOperand)));
        assert_eq!(registers[1], Value::boolean(true));
    }

    #[test]
    fn binding_rejects_an_unknown_function_identity() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut stack = crate::test_support::FrameChainFixture::new();
        let context = ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("runtime-call-unknown-function-test.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture");
        let mut registers = [Value::undefined()];
        let mut frame = Frame::new(
            VmFrameHeader {
                function_id: u32::MAX,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(u32::MAX),
            Value::undefined(),
        );
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));

        assert!(matches!(
            // SAFETY: pointers are live; the test deliberately supplies a
            // function identity absent from the linked execution context.
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) },
            Err(VmError::InvalidOperand)
        ));
    }

    #[test]
    fn runtime_call_uses_the_physical_frame_without_writeback() {
        let (context, function) = bind_this_fixture();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let arguments = vm.alloc_host_object_with_roots(&[], &[]).unwrap();
        let mut frame = vm.test_frame_for_function(&function).unwrap();
        frame.set_new_target(Value::function(99));
        frame.set_derived_constructor();
        let mut stack = crate::test_support::FrameChainFixture::new();
        stack.push(frame);
        let physical = std::ptr::from_mut(&mut stack[0]);
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));
        let mut call = unsafe {
            RuntimeCall::bind(
                NonNull::from(&mut activation),
                NonNull::new(physical).unwrap(),
            )
        }
        .unwrap();
        call.with_frame(|frame| {
            frame.set_native_arguments_object(arguments)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(std::ptr::from_ref(&stack[0]), physical.cast_const());
        assert_eq!(stack[0].new_target(), Value::function(99));
        assert!(stack[0].is_derived_constructor());
        assert_eq!(stack[0].arguments_object(), Some(arguments));
    }

    #[test]
    fn committed_throw_preserves_nested_frames_until_local_catch_acknowledges_it() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let exception = Value::number_i32(73);
        vm.set_pending_uncaught_throw(exception);
        let nested_frames = vec![crate::StackFrameSnapshot {
            function_id: 91,
            function_name: "nestedGetter".to_string(),
            module: "runtime-call-throw-test.js".to_string(),
            span: (11, 19),

            source_position: None,
        }];
        vm.set_uncaught_frames(nested_frames.clone());
        let _ = vm.err_uncaught("stale committed detail".into());

        let mut stack = crate::test_support::FrameChainFixture::new();
        let context = ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("runtime-call-throw-test.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture");
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));
        let mut registers = [Value::undefined()];
        let mut frame = Frame::new(
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

        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("runtime call");
        assert_eq!(call.take_js_throw(VmError::Uncaught), Ok(exception));
        assert!(vm.pending_uncaught_throw.is_none());
        assert_eq!(
            vm.pending_frames_for_test().map(Vec::as_slice),
            Some(nested_frames.as_slice())
        );
        assert!(vm.error_detail().is_none());

        vm.jit_acknowledge_caught_throw();
        assert!(vm.pending_throw_provenance.is_none());

        let later_exception = Value::number_i32(99);
        let later_frames = vec![crate::StackFrameSnapshot {
            function_id: 92,
            function_name: "catchBody".to_string(),
            module: "runtime-call-throw-test.js".to_string(),
            span: (23, 29),

            source_position: None,
        }];
        vm.set_pending_uncaught_throw(later_exception);
        vm.set_uncaught_frames(later_frames.clone());
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("runtime call after catch acknowledgement");
        assert_eq!(call.take_js_throw(VmError::Uncaught), Ok(later_exception));
        assert_eq!(
            vm.pending_frames_for_test().map(Vec::as_slice),
            Some(later_frames.as_slice())
        );
        assert_ne!(
            vm.pending_frames_for_test().map(Vec::as_slice),
            Some(nested_frames.as_slice())
        );
    }

    #[test]
    fn wrong_committed_site_is_fatal_before_semantic_entry() {
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let before = vm.jit_runtime_stats();
        let mut stack = crate::test_support::FrameChainFixture::new();
        let context = ExecutionContext::from_module(
            crate::test_support::minimal_bytecode_module("runtime-call-wrong-site-test.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture");
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));
        let mut registers = [Value::undefined()];
        let mut frame = Frame::new(
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

        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .expect("runtime call");
        assert!(matches!(
            call.object_protocol_values(Value::undefined(), Value::undefined()),
            Err(CommittedValueError::Fatal(VmError::InvalidOperand))
        ));
        assert_eq!(vm.jit_runtime_stats(), before);
        assert!(vm.pending_uncaught_throw.is_none());
        assert!(vm.pending_throw_provenance.is_none());
        assert!(vm.error_detail().is_none());
    }

    #[test]
    fn committed_bind_this_keeps_the_physical_frame_pc() {
        let (context, function) = bind_this_fixture();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut physical = vm
            .test_frame_for_function(&function)
            .expect("derived frame");
        physical.this_value = Value::hole();
        physical.set_derived_constructor();
        let mut stack = crate::test_support::FrameChainFixture::new();
        stack.push(physical);
        let native = std::ptr::from_mut(&mut stack[0]);
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));
        let bound = Value::number_i32(41);

        let mut call = unsafe {
            RuntimeCall::bind(
                NonNull::from(&mut activation),
                NonNull::new(native).unwrap(),
            )
        }
        .expect("runtime call");
        assert_eq!(
            call.scalar_values(bound, Value::undefined())
                .expect("committed bind"),
            bound
        );
        assert_eq!(unsafe { (*native).header.pc }, 0);
        assert_eq!(stack[0].pc, 0);
        assert_eq!(unsafe { (*native).this_value() }, bound);
        assert_eq!(stack[0].this_value, bound);

        assert!(matches!(
            call.scalar_values(Value::number_i32(42), Value::undefined()),
            Err(CommittedValueError::JavaScript(VmError::ThisUninitialized))
        ));
        assert_eq!(unsafe { (*native).header.pc }, 0);
        assert_eq!(stack[0].pc, 0);
        assert_eq!(unsafe { (*native).this_value() }, bound);
        assert_eq!(stack[0].this_value, bound);
    }

    #[test]
    fn committed_bind_keeps_the_enclosing_derived_frame_uninitialized() {
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
        let context = ExecutionContext::from_module(
            crate::BytecodeModule {
                module: "committed-lexical-bind-this-test.js".to_string(),
                template_sites: Vec::new(),
                source_kind: SourceKind::JavaScript,
                functions: vec![outer_function.clone(), inner_function.clone()],
                constants: Vec::new(),
                module_resolutions: Vec::new(),
                module_inits: Vec::new(),
                function_source: None,
            },
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid bytecode fixture");
        drop(base_context);

        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut outer = vm
            .test_frame_for_function(&outer_function)
            .expect("outer derived frame");
        outer.this_value = Value::hole();
        outer.set_derived_constructor();
        let mut inner = vm
            .test_frame_for_function(&inner_function)
            .expect("nested lexical frame");
        inner.this_value = Value::hole();
        let mut stack = crate::test_support::FrameChainFixture::new();
        stack.push(outer);
        stack.push(inner);
        let native = std::ptr::from_mut(&mut stack[1]);
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));
        let bound = Value::number_i32(71);

        let mut call = unsafe {
            RuntimeCall::bind(
                NonNull::from(&mut activation),
                NonNull::new(native).unwrap(),
            )
        }
        .expect("nested runtime call");
        assert_eq!(call.frame_index().unwrap(), 1);
        // A frame-held `this` binds only in its own derived constructor: an
        // arrow's `super()` binds a context slot instead, so a nested
        // non-derived frame is a catchable error and the outer binding stays
        // in its TDZ.
        assert!(matches!(
            call.scalar_values(bound, Value::undefined()),
            Err(CommittedValueError::JavaScript(VmError::ThisUninitialized))
        ));
        assert_eq!(unsafe { (*native).header.pc }, 0);
        assert!(stack[0].this_value.is_hole());
        assert!(stack[1].this_value.is_hole());
        assert!(unsafe { (*native).this_value() }.is_hole());
    }

    #[test]
    fn wrong_frame_bind_this_is_fatal_before_a_local_handler_or_effect() {
        let (context, _) = bind_this_fixture();
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let before = vm.jit_runtime_stats();
        let mut stack = crate::test_support::FrameChainFixture::new();
        let mut registers = [Value::undefined()];
        let mut native = Frame::new(
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

        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, Some(&context));

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
