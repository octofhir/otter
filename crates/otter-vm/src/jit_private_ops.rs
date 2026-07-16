//! Compiled private-member access transitions.
//!
//! # Contents
//! - Single-implementation register helpers for `PrivateGet`, `PrivateSet`, and
//!   `PrivateBrandCheck`, shared by the interpreter dispatch and the compiled
//!   transition.
//! - The reentrant private transition dispatching those three opcodes.
//!
//! # Invariants
//! - No private-element semantics are duplicated in JIT code; each opcode calls
//!   the same VM register helper the interpreter dispatches.
//! - Accessor getter/setter reentry runs through the shared ActivationStack/VmThread
//!   path; a committed private effect is never replayed by an exact side exit.
//!
//! # See also
//! - [`crate::Interpreter::private_element_lookup`]

use otter_bytecode::Op;
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Interpreter, Value, VmError, VmPropertyKey,
    activation_stack::ActivationStack, object, read_register, write_register,
};

impl Interpreter {
    /// §7.3.31 PrivateGet — brand check, then read the field or run the getter.
    pub(crate) fn run_private_get_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        dst: u16,
        obj_reg: u16,
        key_reg: u16,
    ) -> Result<(), VmError> {
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let key = *read_register(&stack[top_idx], key_reg)?;
        let Some(sym) = key.as_symbol(&self.gc_heap) else {
            return Err(self.err_type(
                ("Cannot read private member from an object whose class did not declare it"
                    .to_string())
                .into(),
            ));
        };
        let found = self.private_element_lookup(context, &receiver, sym)?;
        let result = match found {
            None => {
                return Err(self.err_type(
                    ("Cannot read private member from an object whose class did not declare it"
                        .to_string())
                    .into(),
                ));
            }
            Some((_, desc)) => match desc.kind {
                object::DescriptorKind::Data { value } => value,
                object::DescriptorKind::Accessor { getter, .. } => match getter {
                    Some(getter) => {
                        self.run_callable_sync(context, &getter, receiver, SmallVec::new())?
                    }
                    None => {
                        return Err(
                            self.err_type(("'#x' was defined without a getter".to_string()).into())
                        );
                    }
                },
            },
        };
        let frame = &mut stack[top_idx];
        write_register(frame, dst, result)?;
        frame.advance_pc()?;
        Ok(())
    }

    /// §7.3.32 PrivateSet — brand check, method/accessor rules, own-field write.
    pub(crate) fn run_private_set_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        obj_reg: u16,
        key_reg: u16,
        value_reg: u16,
    ) -> Result<(), VmError> {
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let key = *read_register(&stack[top_idx], key_reg)?;
        let value = *read_register(&stack[top_idx], value_reg)?;
        let Some(sym) = key.as_symbol(&self.gc_heap) else {
            return Err(self.err_type(
                ("Cannot write private member to an object whose class did not declare it"
                    .to_string())
                .into(),
            ));
        };
        let found = self.private_element_lookup(context, &receiver, sym)?;
        match found {
            None => {
                return Err(self.err_type(
                    ("Cannot write private member to an object whose class did not declare it"
                        .to_string())
                    .into(),
                ));
            }
            Some((holder, desc)) => match desc.kind {
                object::DescriptorKind::Accessor { setter, .. } => match setter {
                    Some(setter) => {
                        let argv: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        self.run_callable_sync(context, &setter, receiver, argv)?;
                    }
                    None => {
                        return Err(
                            self.err_type(("'#x' was defined without a setter".to_string()).into())
                        );
                    }
                },
                object::DescriptorKind::Data { .. } => {
                    if holder != receiver || !desc.flags.writable() {
                        return Err(
                            self.err_type(("Private method is not writable".to_string()).into())
                        );
                    }
                    let descriptor = object::PartialPropertyDescriptor {
                        value: Some(value),
                        ..Default::default()
                    };
                    let vm_key = VmPropertyKey::Symbol(sym);
                    self.define_own_property_value(context, &receiver, &vm_key, descriptor)?;
                }
            },
        }
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    /// §7.3.31 PrivateElementFind own-only brand check.
    pub(crate) fn run_private_brand_check_reg(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        top_idx: usize,
        obj_reg: u16,
        brand_reg: u16,
    ) -> Result<(), VmError> {
        let receiver = *read_register(&stack[top_idx], obj_reg)?;
        let brand = *read_register(&stack[top_idx], brand_reg)?;
        let Some(sym) = brand.as_symbol(&self.gc_heap) else {
            return Err(self.err_type(
                ("Cannot read private member from an object whose class did not declare it"
                    .to_string())
                .into(),
            ));
        };
        let key = VmPropertyKey::Symbol(sym);
        let found = if let Some(p) = receiver.as_proxy() {
            self.proxy_private_find(&p, sym).is_some()
        } else {
            self.ordinary_get_own_property_descriptor_value_runtime_rooted(
                context,
                receiver,
                &key,
                0,
                &[&receiver, &brand],
                &[],
            )?
            .is_some()
        };
        if !found {
            return Err(self.err_type(
                ("Cannot read private member from an object whose class did not declare it"
                    .to_string())
                .into(),
            ));
        }
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    /// Complete one private-member opcode for a published compiled frame.
    pub fn jit_runtime_private_op(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        if frame_index + 1 != stack.len() {
            return Err(VmError::InvalidOperand);
        }
        let saved_pc = stack[frame_index].pc;
        match opcode {
            value if value == Op::PrivateGet as u8 => {
                self.run_private_get_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                    arg2 as u16,
                )?;
            }
            value if value == Op::PrivateSet as u8 => {
                self.run_private_set_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                    arg2 as u16,
                )?;
            }
            value if value == Op::PrivateBrandCheck as u8 => {
                self.run_private_brand_check_reg(
                    context,
                    stack,
                    frame_index,
                    arg0 as u16,
                    arg1 as u16,
                )?;
            }
            _ => return Err(VmError::InvalidOperand),
        }
        stack[frame_index].pc = saved_pc;
        Ok(())
    }
}
