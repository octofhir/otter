//! Owned input to a native-stack JavaScript activation.
//!
//! # Contents
//! - [`PreparedCall`] holds actual arguments and optional restored registers.
//! - Packet construction for the common native trampoline.
//! - Tracing while a request is queued but has not entered its native frame.
//!
//! # Invariants
//! A prepared call owns input values, never an active frame or register window.
//! Its buffers stay rooted until the trampoline has copied them. Ordinary
//! calls seed formals directly from actuals; register seeds are reserved for
//! restored or explicitly initialized entry state. Cold identity is transferred
//! exactly once to the frame created by the trampoline.
//!
//! # See also
//! - [`crate::native_abi::CallRequest`] for the machine-readable packet.
//! - [`crate::activation_stack::ActivationStack`] for request publication.
//! - [`crate::frame_state::ParkedFrameState`] for suspended ownership.

use crate::{
    CodeBlock, JsObject, Value, VmError,
    cold_frame::ColdFrameIdx,
    native_abi::{CallRequest, NativeFrameFlags, VmFrameHeader},
};
use otter_gc::raw::SlotVisitor;
use smallvec::SmallVec;

#[derive(Debug, Default)]
pub(crate) enum ResumeInput {
    #[default]
    Normal,
    Throw(Value),
}

#[derive(Debug)]
pub(crate) struct PreparedCall {
    pub(crate) header: VmFrameHeader,
    pub(crate) self_value: Value,
    pub(crate) this_value: Value,
    pub(crate) new_target_value: Value,
    pub(crate) arguments_object: JsObject,
    pub(crate) construct_layout: crate::constructor_layout::ConstructorLayout,
    pub(crate) derived_this_context: crate::context::ContextHandle,
    pub(crate) construct_receiver: Value,
    pub(crate) return_register: Option<u16>,
    pub(crate) cold: Option<ColdFrameIdx>,
    pub(crate) parameter_count: u16,
    pub(crate) arguments: SmallVec<[Value; 8]>,
    pub(crate) initial_registers: SmallVec<[Value; 8]>,
    pub(crate) child: Option<Box<PreparedCall>>,
    pub(crate) resume: ResumeInput,
}

impl PreparedCall {
    pub(crate) fn for_code_block(
        function: &CodeBlock,
        return_register: Option<u16>,
        self_value: Value,
        this_value: Value,
    ) -> Self {
        Self {
            header: VmFrameHeader::interpreter(function.id, function.register_count),
            self_value,
            this_value,
            new_target_value: Value::UNDEFINED,
            arguments_object: JsObject::null(),
            construct_layout: crate::constructor_layout::ConstructorLayout::null(),
            derived_this_context: crate::context::ContextHandle::null(),
            construct_receiver: Value::UNDEFINED,
            return_register,
            cold: None,
            parameter_count: function.param_count,
            arguments: SmallVec::new(),
            initial_registers: SmallVec::new(),
            child: None,
            resume: ResumeInput::Normal,
        }
    }

    pub(crate) fn packet(&self) -> CallRequest {
        CallRequest {
            entry: crate::interp::call_dispatch::interpreter_entry as *const () as u64,
            header: self.header,
            code_object_id: 0,
            arguments: self.arguments.as_ptr(),
            argument_count: self.arguments.len() as u32,
            parameter_count: u32::from(self.parameter_count),
            callee: self.self_value,
            receiver: self.this_value,
            new_target: self.new_target_value,
            initial_registers: self.initial_registers.as_ptr(),
            initial_register_count: self.initial_registers.len() as u32,
            return_destination: self.return_register.map_or(u32::MAX, u32::from),
            cold: self.cold,
            arguments_object: self.arguments_object,
            construct_layout: self.construct_layout,
            derived_this_context: self.derived_this_context,
            construct_receiver: self.construct_receiver,
            // Parked ownership cannot retain an ancestor's native address.
            super_origin: 0,
            caller: 0,
            caller_return_pc: 0,
        }
    }

    pub(crate) fn seed_register(&mut self, register: u16, value: Value) -> Result<(), VmError> {
        if register >= self.header.register_count {
            return Err(VmError::InvalidOperand);
        }
        let index = usize::from(register);
        if self.initial_registers.len() <= index {
            self.initial_registers.resize(index + 1, Value::UNDEFINED);
        }
        self.initial_registers[index] = value;
        Ok(())
    }

    pub(crate) fn set_new_target(&mut self, value: Value) {
        self.new_target_value = value;
    }
    pub(crate) fn set_construct(&mut self) {
        self.header.flags =
            NativeFrameFlags::from_bits(self.header.flags.bits() | NativeFrameFlags::CONSTRUCT);
    }
    pub(crate) fn set_derived_constructor(&mut self) {
        self.header.flags = NativeFrameFlags::from_bits(
            self.header.flags.bits()
                | NativeFrameFlags::CONSTRUCT
                | NativeFrameFlags::DERIVED_CONSTRUCTOR,
        );
    }

    fn trace_inputs(&self, visitor: &mut SlotVisitor<'_>) {
        match &self.resume {
            ResumeInput::Throw(value) => value.trace_value_slots(visitor),
            ResumeInput::Normal => {}
        }
        self.self_value.trace_value_slots(visitor);
        self.this_value.trace_value_slots(visitor);
        self.new_target_value.trace_value_slots(visitor);
        self.construct_receiver.trace_value_slots(visitor);
        if !self.derived_this_context.is_null() {
            visitor(
                std::ptr::addr_of!(self.derived_this_context)
                    .cast_mut()
                    .cast(),
            );
        }
        if !self.construct_layout.is_null() {
            visitor(std::ptr::addr_of!(self.construct_layout).cast_mut().cast());
        }
        for value in self.arguments.iter().chain(self.initial_registers.iter()) {
            value.trace_value_slots(visitor);
        }
        if !self.arguments_object.is_null() {
            visitor(std::ptr::addr_of!(self.arguments_object).cast_mut().cast());
        }
    }
    pub(crate) fn trace_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let mut call = Some(self);
        while let Some(current) = call {
            current.trace_inputs(visitor);
            call = current.child.as_deref();
        }
    }
}

impl std::ops::Deref for PreparedCall {
    type Target = VmFrameHeader;
    fn deref(&self) -> &Self::Target {
        &self.header
    }
}
impl std::ops::DerefMut for PreparedCall {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.header
    }
}

impl otter_gc::ExtraRootSource for PreparedCall {
    fn visit_extra_roots(&self, visitor: &mut SlotVisitor<'_>) {
        self.trace_slots(visitor);
    }
}
