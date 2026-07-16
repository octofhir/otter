//! Typed, allocation-free access to one compiled VM activation.
//!
//! # Contents
//! - [`RuntimeCall`] is the short-lived JIT-to-VM semantic boundary.
//! - A private frame identity distinguishes an interpreter-owned root from a
//!   frameless native-call owner without exposing either container.
//! - Focused `control` and `value_ops` implementations expose typed
//!   operations instead of raw interpreter, stack, context, or frame handles.
//!
//! # Invariants
//! - Construction copies and validates raw descriptors from the published
//!   runtime record and current [`NativeFrame`]; the boundary retains only
//!   `NonNull` identities, never references to either owner container.
//! - No method returns the interpreter, materialized stack, native frame,
//!   register pointer, or an [`ActiveFrameMut`] view. Frame views exist only
//!   inside one typed method and never survive a VM transition.
//! - A semantic operation may open the VM/context reference required by the
//!   existing exclusive-mutator contract. Native-frame and stack slot views
//!   remain operation-scoped and are not retained across that call.
//! - A native frame's activation word is decoded once. The few genuinely
//!   materialized-only cold operations reject a native owner before mutating
//!   state; total value-level operations never side-exit for representation.
//! - Register/upvalue windows stay published across allocating operations;
//!   slot access remains checked and scoped through [`ActiveFrameMut`].
//! - The boundary allocates no wrapper, lock, side table, or thread-local state.
//!
//! # See also
//! - [`crate::jit::VmRuntimeActivation`] owns the opaque entry-lifetime record.
//! - [`crate::active_frame`] validates the machine-published frame windows.

mod control;
mod value_ops;

use std::{marker::PhantomData, ptr::NonNull};

use crate::{
    ActiveFrameMut, ActiveFrameRef, ExecutionContext, HoltStack, Interpreter, Value, VmError,
    jit::VmRuntimeActivation,
    native_abi::{NativeFrame, NativeFrameFlags},
};

/// Ownership identity carried by the current native frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeFrameIdentity {
    /// Compiled view of an existing interpreter activation.
    Materialized(u32),
    /// VM-owned resources of a frameless compiled callee.
    NativeOwner(u32),
}

/// Exclusive, short-lived semantic view of one compiled activation.
///
/// This type deliberately owns only branded raw descriptors to VM services.
/// JIT stubs can request operations, read/write one checked register, and
/// inspect scalar frame identity only. In particular, no retained native-frame
/// or materialized-stack reference aliases the GC's active-frame root walk.
pub struct RuntimeCall<'a> {
    pub(super) vm: NonNull<Interpreter>,
    pub(super) stack: NonNull<HoltStack>,
    pub(super) context: NonNull<ExecutionContext>,
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
        let context = NonNull::new(activation.context.cast_mut()).ok_or(VmError::InvalidOperand)?;
        // Validate both published windows before exposing any semantic method.
        // SAFETY: the entry contract retains the initialized raw descriptor for
        // `'a`; ActiveFrameRef itself stores no native Rust reference.
        unsafe { ActiveFrameRef::from_native_ptr(frame.as_ptr()) }
            .map_err(|_| VmError::InvalidOperand)?;
        // SAFETY: one copied descriptor snapshot; it dies before any semantic
        // operation or safepoint.
        let frame_snapshot = unsafe { frame.as_ptr().read() };
        let identity = if frame_snapshot
            .header
            .flags
            .contains(NativeFrameFlags::MATERIALIZED)
        {
            let frame_index = frame_snapshot.activation_id as usize;
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
            if materialized.function_id != frame_snapshot.header.function_id
                || materialized.registers.len() != usize::from(frame_snapshot.header.register_count)
                || materialized.registers.as_mut_ptr() as u64 != frame_snapshot.register_base
                || expected_upvalue_base != frame_snapshot.upvalue_base
                || materialized.upvalues.len() != frame_snapshot.upvalue_count as usize
            {
                return Err(VmError::InvalidOperand);
            }
            RuntimeFrameIdentity::Materialized(frame_snapshot.activation_id)
        } else {
            RuntimeFrameIdentity::NativeOwner(frame_snapshot.activation_id)
        };
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

    pub(super) fn materialized_index(&self) -> Option<usize> {
        match self.identity {
            RuntimeFrameIdentity::Materialized(index) => Some(index as usize),
            RuntimeFrameIdentity::NativeOwner(_) => None,
        }
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

    #[test]
    fn identity_is_decoded_once_and_slot_access_is_checked() {
        let mut vm = Interpreter::new();
        let mut stack = HoltStack::new();
        let context = ExecutionContext::from_module(crate::BytecodeModule {
            module: "runtime-call-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: otter_bytecode::SourceKind::TypeScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        });
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
        let mut registers = [Value::number_i32(3), Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 7,
                code_block_id: 7,
                pc: 11,
                register_count: 2,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        frame.set_native_owner(41);

        // SAFETY: the local activation, frame, and register array stay live and
        // exclusively owned for the RuntimeCall scope.
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) }
                .unwrap();
        assert_eq!(call.identity(), RuntimeFrameIdentity::NativeOwner(41));
        assert_eq!(call.read(0).unwrap(), Value::number_i32(3));
        call.write(1, Value::boolean(true)).unwrap();
        assert!(matches!(call.read(2), Err(VmError::InvalidOperand)));
        assert_eq!(registers[1], Value::boolean(true));
    }

    #[test]
    fn materialized_identity_must_resolve_the_published_stack_slot() {
        let mut vm = Interpreter::new();
        let mut stack = HoltStack::new();
        let context = ExecutionContext::from_module(crate::BytecodeModule {
            module: "runtime-call-materialized-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: otter_bytecode::SourceKind::TypeScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        });
        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 7,
                code_block_id: 7,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Baseline,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        frame.set_materialized_activation(0);
        let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);

        assert!(matches!(
            // SAFETY: pointers are live; the test deliberately supplies a
            // materialized identity that cannot resolve in the empty stack.
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame)) },
            Err(VmError::InvalidOperand)
        ));
    }
}
