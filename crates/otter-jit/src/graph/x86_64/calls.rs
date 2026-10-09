//! x86 Graph calls over the common JS and committed runtime boundaries.
//!
//! # Contents
//! - Canonical collecting argument staging and late result commitment.
//! - Current-generation Known and native-kind calls, classification and forwarding.
//! - Reusable baseline operations on the published interpreter window.
//!
//! # Invariants
//! - Live values enter canonical homes before collection or JS reentry.
//! - Actual spans contain only actuals; the callee owns missing formals.
//! - Result registers are assigned only after guard/call completion succeeds.
//! - Every C transition uses the shared platform adapter and VM descriptor.
//!
//! # See also
//! - `crate::x86_64::js_call` owns the private JS physical call convention.
//! - `crate::template::x86_64::operation` owns baseline operation encoding.

use super::*;
use crate::entry::VALUE_UNDEFINED;
use crate::x86_64::js_call::{CallTarget, emit_call};

impl Codegen<'_> {
    pub(super) fn emit_committed_call(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        arguments: &[CommittedArgument],
        destination: Option<u8>,
    ) -> Result<(), Unsupported> {
        self.emit_committed_call_inner(node, stub, arguments, destination, 0)
    }
    pub(super) fn emit_committed_span_call(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        values: &[Location],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let bytes = self.emit_push_actuals(values)?;
        self.emit_committed_call_inner(
            node,
            stub,
            &[
                CommittedArgument::StackAddress(0),
                CommittedArgument::Scalar(values.len() as u64),
            ],
            Some(destination),
            bytes,
        )
    }
    pub(super) fn emit_committed_call_inner(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        arguments: &[CommittedArgument],
        destination: Option<u8>,
        span_bytes: u32,
    ) -> Result<(), Unsupported> {
        assert!(arguments.len() <= 3);
        self.save_live_homes(node);
        self.stamp_node_safepoint(node)?;
        self.stamp_pc(node);
        // Values may sit in any argument register: stage the first two in the
        // scratch pair and a third on the stack before any is overwritten.
        let mut staged = 0;
        for argument in arguments {
            if let CommittedArgument::Value(value) = *argument {
                if staged < 2 {
                    dynasm!(self.ops ; .arch x64 ; mov Rq(10 + staged), Rq(value));
                } else {
                    dynasm!(self.ops ; .arch x64 ; push Rq(value));
                }
                staged += 1;
            }
        }
        dynasm!(self.ops ; .arch x64 ; mov rdi, r15);
        staged = 0;
        for (index, argument) in arguments.iter().enumerate() {
            let register = [6, 2, 1][index];
            match argument {
                CommittedArgument::Value(_) => {
                    if staged < 2 {
                        dynasm!(self.ops ; .arch x64 ; mov Rq(register), Rq(10 + staged));
                    } else {
                        dynasm!(self.ops ; .arch x64 ; pop Rq(register));
                    }
                    staged += 1;
                }
                CommittedArgument::Scalar(bits) => self.load_immediate(register, *bits),
                CommittedArgument::Address(bits, target) => emit_load_symbol_u64(
                    &mut self.ops,
                    &mut self.relocations,
                    register,
                    *bits,
                    target.clone(),
                ),
                CommittedArgument::StackAddress(offset) => {
                    dynasm!(self.ops ; .arch x64 ; lea Rq(register), [rsp + *offset as i32])
                }
            }
        }
        let (_, committed_throw) = self.throw_targets(node);
        let fatal = self.fatal;
        let success = self.ops.new_dynamic_label();
        self.emit_stub_call(stub);
        // The stub copied a span before it could collect; discard the stale
        // packet before all status and recovery paths use canonical homes.
        self.emit_pop_actuals(span_bytes);
        dynasm!(self.ops ; .arch x64 ; test rdx, rdx ; jz =>success
            ; cmp edx, abi::NativeResultStatus::Throw as i32 ; je =>committed_throw ; jmp =>fatal
            ; =>success ; mov r11, rax
        );
        self.restore_live_homes(node);
        if let Some(dst) = destination {
            dynasm!(self.ops ; .arch x64 ; mov Rq(dst), r11);
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_call_js(
        &mut self,
        node: NodeId,
        pc: u32,
        plan: crate::call_linkage::CallPlan,
        construct: bool,
        receiver: bool,
        allocation: Option<otter_vm::jit::JitReceiverAllocationPlan>,
    ) -> Result<(), Unsupported> {
        self.node_safepoint(node)?;
        let inputs = self.loc(node).inputs.clone();
        let arguments = &inputs[if receiver { 2 } else { 1 }..];
        let bytes = self.emit_push_actuals(arguments)?;
        let count = u32::try_from(arguments.len())
            .map_err(|_| Unsupported::OperandShape("actual count"))?;
        // Fixed callee/receiver registers are already supplied by allocation.
        if construct {
            dynasm!(self.ops ; .arch x64 ; mov rcx, rsi);
        }
        // Construction must clear the request before receiver preparation.
        // Ordinary calls are cleared by the shared call emitter on every target.
        if construct {
            crate::x86_64::js_call::emit_clear_construct_ticket(&mut self.ops, 15);
        }
        let generic = self.ops.new_dynamic_label();
        let returned = self.ops.new_dynamic_label();
        if let crate::call_linkage::CallPlan::Bytecode(plan) = plan {
            self.emit_function_identity(plan.function_id, plan.callee_cell, pc, 6, generic);
            // Actuals are already rooted in the staged span and every value
            // live after this call has a canonical home. A fit is published
            // immediately before the callee publishes it as Frame.this.
            let known_receiver = if let Some(allocation) = allocation {
                debug_assert!(construct && !receiver && !plan.is_derived_constructor);
                let source = self.view_of(node);
                let missed = self.ops.new_dynamic_label();
                let allocated = self.ops.new_dynamic_label();
                crate::x86_64::allocation::emit_receiver_candidate_probe(
                    &mut self.ops,
                    &mut self.relocations,
                    source,
                    allocation,
                    15,
                );
                dynasm!(self.ops ; .arch x64 ; test rdx, rdx ; jz =>missed);
                crate::x86_64::allocation::emit_receiver_publication_effect(
                    &mut self.ops,
                    source,
                    15,
                );
                dynasm!(self.ops ; .arch x64 ; mov rdx, rax ; jmp =>allocated
                    ; =>missed ; mov edx, VALUE_UNDEFINED as i32 ; =>allocated);
                true
            } else {
                receiver
            };
            let return_pc = emit_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                15,
                known_receiver,
                construct,
                count,
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch x64 ; jmp =>returned);
        }
        if matches!(plan, crate::call_linkage::CallPlan::Native) {
            crate::x86_64::js_call::emit_native_kind_guard(&mut self.ops, 6, generic);

            let return_pc = emit_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                15,
                receiver,
                construct,
                count,
                CallTarget::Native,
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch x64 ; jmp =>returned);
        }
        dynasm!(self.ops ; .arch x64 ; =>generic);
        let return_pc = emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            15,
            receiver,
            construct,
            count,
            CallTarget::Generic,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops ; .arch x64 ; =>returned);
        self.emit_pop_actuals(bytes);
        self.emit_call_completion(node)?;
        Ok(())
    }
    pub(super) fn emit_call_forward(
        &mut self,
        node: NodeId,
        pc: u32,
        plan: crate::call_linkage::CallPlan,
        bindings: &[u16],
    ) -> Result<(), Unsupported> {
        use crate::call_linkage::ForwardedBinding;
        use crate::x86_64::js_call::{
            emit_forwarded_call, emit_pop_forwarded, emit_push_forwarded,
        };
        let overflow = self.eager_exit(node, DeoptReason::WrongValue);
        self.node_safepoint(node)?;
        let allocation = self.loc(node);
        let temps = [allocation.gp_temps[0], allocation.gp_temps[1]];
        let mapped = bindings
            .iter()
            .zip(&allocation.inputs[2..])
            .map(|(&index, &location)| {
                let binding = match location {
                    Location::Gp(register) => ForwardedBinding::Register(register),
                    Location::TaggedSlot(_) | Location::UntaggedSlot(_) => ForwardedBinding::Load {
                        base: temps[0],
                        offset: self.slots.offset(location) + self.sp_delta,
                    },
                    Location::Constant(value) => {
                        ForwardedBinding::Immediate(self.constant_bits(value))
                    }
                    Location::Fp(_) => unreachable!("tagged actual"),
                };
                (index, binding)
            })
            .collect::<Vec<_>>();
        emit_push_forwarded(&mut self.ops, 14, temps, overflow, &mapped);
        let generic = self.ops.new_dynamic_label();
        let returned = self.ops.new_dynamic_label();
        if let crate::call_linkage::CallPlan::Bytecode(plan) = plan {
            self.emit_function_identity(plan.function_id, plan.callee_cell, pc, 6, generic);
            let return_pc = emit_forwarded_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                15,
                CallTarget::Known {
                    entry_cell: plan.entry_cell,
                    function_id: plan.function_id,
                },
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch x64 ; jmp =>returned);
        }
        if matches!(plan, crate::call_linkage::CallPlan::Native) {
            crate::x86_64::js_call::emit_native_kind_guard(&mut self.ops, 6, generic);

            let return_pc = emit_forwarded_call(
                &mut self.ops,
                &mut self.relocations,
                self.transitions,
                15,
                CallTarget::Native,
            );
            self.record_js_return(node, return_pc)?;
            dynasm!(self.ops ; .arch x64 ; jmp =>returned);
        }
        dynasm!(self.ops ; .arch x64 ; =>generic);
        let return_pc = emit_forwarded_call(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            15,
            CallTarget::Generic,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops ; .arch x64 ; =>returned);
        emit_pop_forwarded(&mut self.ops, 14);
        self.emit_call_completion(node)?;
        Ok(())
    }
    pub(super) fn emit_call_completion(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let (threw, committed) = self.throw_targets(node);
        let completion = self.ops.new_dynamic_label();
        let error = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64 ; =>completion ; test rdx, rdx ; jz =>done
            ; cmp edx, abi::NativeResultStatus::Continue as i32 ; jne =>error);
        let return_pc = crate::x86_64::js_call::emit_enter_staged(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            15,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops ; .arch x64 ; jmp =>completion ; =>error);
        self.stamp_node_safepoint(node)?;
        self.stamp_pc(node);
        // A callee's Fatal is final: no handler may observe it and its parked
        // error must not be projected into an exception again.
        let fatal = self.fatal;
        dynasm!(self.ops ; .arch x64
            ; cmp edx, abi::NativeResultStatus::Throw as i32 ; je =>committed
            ; cmp edx, abi::NativeResultStatus::Fatal as i32 ; je =>fatal
            ; jmp =>threw ; =>done);
        Ok(())
    }
    pub(super) fn emit_generic(
        &mut self,
        node: NodeId,
        pc: u32,
        registers: &[u16],
    ) -> Result<(), Unsupported> {
        debug_assert_eq!(
            self.graph.node(node).origin,
            0,
            "validated baseline window owner"
        );
        debug_assert_eq!(
            pc,
            self.graph.outer_pc(node),
            "Generic physical source is outermost"
        );
        let source = self.view_of(node);
        let call_safepoint = self.node_safepoint(node)?;
        let inputs = self.loc(node).inputs.clone();
        for (&register, &location) in registers.iter().zip(&inputs) {
            self.emit_move(location, Location::Gp(10));
            dynasm!(self.ops ; .arch x64 ; mov [r13 + i32::from(register) * 8], r10);
        }
        let (threw, committed_throw) = self.throw_targets(node);
        let runtime_transition = self.typed_exit(
            node,
            abi::ExitReason::RuntimeTransition,
            abi::ExitAction::Resume,
        );
        let exits = crate::template::operation::OperationExits {
            type_mismatch_exit: self.typed_exit(
                node,
                abi::ExitReason::TypeMismatch,
                abi::ExitAction::Recompile,
            ),
            allocation_miss_exit: self.typed_exit(
                node,
                abi::ExitReason::AllocationMiss,
                abi::ExitAction::Resume,
            ),
            unsupported_exit: self.typed_exit(
                node,
                abi::ExitReason::UnsupportedOperation,
                abi::ExitAction::Recompile,
            ),
            runtime_transition_exit: runtime_transition,
            backedge_relink_exit: self.typed_exit(
                node,
                abi::ExitReason::Interrupt,
                abi::ExitAction::Resume,
            ),
            returned: self.returned,
            committed_throw,
            threw,
            fatal: self.fatal,
        };
        let positions = self.plan_index.get(&pc).cloned().unwrap_or_default();
        let labels = std::collections::BTreeMap::new();
        for position in positions {
            let operation = self.plan.instructions[position].op;
            if !crate::template::operation_is_js_call(operation) {
                self.stamp_node_safepoint(node)?;
                self.stamp_pc(node);
            }

            crate::template::x86_64::operation::emit_operation(
                crate::template::x86_64::operation::OperationContext {
                    ops: &mut self.ops,
                    relocations: &mut self.relocations,
                    return_sites: &mut self.return_sites,
                    call_safepoint,
                    transitions: self.transitions,
                    view: source,
                    plan: self.plan,
                    labels: &labels,
                    exits,
                    frame_kind: abi::NativeFrameKind::Optimizing,
                    shared_property: &mut self.shared_property,
                    direct_call_events: &mut self.no_direct_call_events,
                    code_map: &mut self.no_code_map,
                },
                &self.plan.instructions[position],
            )?;
        }
        Ok(())
    }
}
