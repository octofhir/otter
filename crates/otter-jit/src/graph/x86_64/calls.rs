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

/// What a committed call enters: a runtime stub, or a routine of the
/// compiler's set that takes the same ABI.
#[derive(Clone, Copy)]
pub(super) enum CommittedTarget {
    Stub(abi::RuntimeStubDescriptor),
    Routine(crate::routines::Routine),
}

impl Codegen<'_> {
    pub(super) fn emit_committed_call(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubDescriptor,
        arguments: &[CommittedArgument],
        destination: Option<u8>,
    ) -> Result<(), Unsupported> {
        self.emit_committed_call_inner(node, CommittedTarget::Stub(stub), arguments, destination, 0)
    }

    /// [`Self::emit_committed_call`] of a routine of the compiler's set that
    /// takes the committed runtime ABI.
    pub(super) fn emit_committed_routine_call(
        &mut self,
        node: NodeId,
        routine: crate::routines::Routine,
        arguments: &[CommittedArgument],
        destination: Option<u8>,
    ) -> Result<(), Unsupported> {
        self.emit_committed_call_inner(
            node,
            CommittedTarget::Routine(routine),
            arguments,
            destination,
            0,
        )
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
            CommittedTarget::Stub(stub),
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
        target: CommittedTarget,
        arguments: &[CommittedArgument],
        destination: Option<u8>,
        span_bytes: u32,
    ) -> Result<(), Unsupported> {
        assert!(arguments.len() <= 4);
        self.save_live_homes(node);
        self.stamp_node_safepoint(node)?;
        self.stamp_pc(node);
        // Values may sit in any argument register: stage the first two in the
        // scratch pair and the rest on the stack before any is overwritten.
        let values = arguments
            .iter()
            .filter(|argument| matches!(argument, CommittedArgument::Value(_)))
            .count();
        let pushed = values.saturating_sub(2) as i32;
        debug_assert!(
            pushed == 0
                || !arguments
                    .iter()
                    .any(|argument| matches!(argument, CommittedArgument::StackAddress(_))),
            "stack-staged values would move a stack-address argument"
        );
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
            let register = [6, 2, 1, 8][index];
            match argument {
                CommittedArgument::Value(_) => {
                    if staged < 2 {
                        dynasm!(self.ops ; .arch x64 ; mov Rq(register), Rq(10 + staged));
                    } else {
                        // The last value pushed sits lowest.
                        let offset = (pushed - 1 - (i32::from(staged) - 2)) * 8;
                        dynasm!(self.ops ; .arch x64 ; mov Rq(register), [rsp + offset]);
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
        if pushed != 0 {
            dynasm!(self.ops ; .arch x64 ; add rsp, pushed * 8);
        }
        let (_, committed_throw) = self.throw_targets(node);
        let fatal = self.fatal;
        let success = self.ops.new_dynamic_label();
        match target {
            CommittedTarget::Stub(stub) => self.emit_stub_call(stub),
            CommittedTarget::Routine(routine) => {
                crate::template::x86_64::shared_property::emit_call(
                    &mut self.ops,
                    &mut self.relocations,
                    self.routines,
                    routine,
                );
            }
        }
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
        receiver_object: bool,
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
                crate::x86_64::allocation::emit_receiver_publication_effect(&mut self.ops, 15);
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
                    receiver_ready: receiver_object,
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
        let generic_call = move |codegen: &mut Codegen<'_>| {
            dynasm!(codegen.ops ; .arch x64 ; =>generic);
            let return_pc = emit_call(
                &mut codegen.ops,
                &mut codegen.relocations,
                codegen.transitions,
                15,
                receiver,
                construct,
                count,
                CallTarget::Generic,
            );
            codegen.record_js_return(node, return_pc)
        };
        if matches!(plan, crate::call_linkage::CallPlan::Generic) {
            generic_call(self)?;
        } else {
            // A proved target makes the generic entry the cold path.
            self.defer(move |codegen| {
                generic_call(codegen)?;
                dynasm!(codegen.ops ; .arch x64 ; jmp =>returned);
                Ok(())
            });
        }
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
                    receiver_ready: false,
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
    /// The call of `CallApplyArray` `node`: the callee in `rsi` with the
    /// receiver in `rdx` and the elements of its list as actual arguments,
    /// copied into a span of [`APPLY_ARRAY_MAX`] slots. Every proof about
    /// the list precedes the span: a list that is not a plain dense Array
    /// of at most that many present elements in tagged or packed numeric
    /// storage deopts before any effect.
    pub(super) fn emit_call_apply_array(&mut self, node: NodeId) -> Result<(), Unsupported> {
        use crate::graph::ir::APPLY_ARRAY_MAX;
        use crate::x86_64::js_call::emit_forwarded_call;
        use otter_vm::jit::JitArrayStorage;
        const SPAN_BYTES: i32 = (APPLY_ARRAY_MAX * 8) as i32;
        let storage = JitArrayStorage::current();
        let numeric = storage.numeric;
        let exit = self.eager_exit(node, DeoptReason::WrongValue);
        self.node_safepoint(node)?;
        let allocation = self.loc(node);
        let [list, base, count] = [
            allocation.gp_temps[0],
            allocation.gp_temps[1],
            allocation.gp_temps[2],
        ];
        match allocation.inputs[2] {
            Location::Gp(register) => dynasm!(self.ops ; .arch x64 ; mov Rq(list), Rq(register)),
            slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)) => {
                self.load_slot_gp(list, slot);
            }
            Location::Constant(value) => {
                let bits = self.constant_bits(value);
                self.load_immediate(list, bits);
            }
            Location::Fp(_) => unreachable!("an apply list is tagged"),
        }
        let tagged = self.ops.new_dynamic_label();
        let scan = self.ops.new_dynamic_label();
        let push_tagged = self.ops.new_dynamic_label();
        let push_packed = self.ops.new_dynamic_label();
        let call = self.ops.new_dynamic_label();
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(list), r10 ; jnz =>exit
            ; test Rq(list), Rq(list) ; jz =>exit
            ; cmp BYTE [Rq(list)], storage.type_tag as i8 ; jne =>exit
            ; cmp BYTE [Rq(list) + storage.own_guard_byte as i32], 0 ; jne =>exit
            // Every element below the length is in the dense prefix.
            ; mov Rd(count), [Rq(list) + storage.length_byte as i32]
            ; cmp Rd(count), [Rq(list) + storage.logical_length_byte as i32] ; jne =>exit
            ; cmp Rd(count), APPLY_ARRAY_MAX as i32 ; ja =>exit
            ; mov Rq(base), [Rq(list) + storage.base_byte as i32]
            ; cmp BYTE [Rq(list) + numeric.kind_byte as i32], storage.tagged_kind as i8 ; je =>tagged
            ; cmp BYTE [Rq(list) + numeric.kind_byte as i32], numeric.packed_kind as i8 ; jne =>exit
            ; jmp =>push_packed
            // A hole reads through the prototype chain.
            ; =>tagged
            ; mov r11d, Rd(count)
        );
        self.load_immediate(10, tag::VALUE_HOLE);
        dynasm!(self.ops ; .arch x64
            ; =>scan
            ; test r11d, r11d ; jz =>push_tagged
            ; dec r11d
            ; cmp [Rq(base) + r11 * 8], r10 ; je =>exit
            ; jmp =>scan
        );
        // Each arm reserves the span past the stack limit check, before which
        // nothing has an effect, then copies the elements; `list` is the
        // copy's index.
        for (arm, packed) in [(push_tagged, false), (push_packed, true)] {
            let copy = self.ops.new_dynamic_label();
            dynasm!(self.ops ; .arch x64
                ; =>arm
                ; lea r10, [rsp - SPAN_BYTES]
                ; cmp r10, [r15 + crate::entry::NATIVE_STACK_LIMIT_OFFSET as i32] ; jb =>exit
                ; mov rsp, r10
                ; xor Rd(list), Rd(list)
                ; =>copy
                ; cmp Rd(list), Rd(count) ; jae =>call
            );
            if packed {
                dynasm!(self.ops ; .arch x64 ; movsd xmm15, [Rq(base) + Rq(list) * 8]);
                crate::x86_64::field_word::emit_encode_double(&mut self.ops, 15, 10, 11);
            } else {
                dynasm!(self.ops ; .arch x64 ; mov r10, [Rq(base) + Rq(list) * 8]);
            }
            dynasm!(self.ops ; .arch x64
                ; mov [rsp + Rq(list) * 8], r10
                ; inc Rd(list)
                ; jmp =>copy
            );
        }
        dynasm!(self.ops ; .arch x64 ; =>call ; mov r8d, Rd(count));
        let return_pc = emit_forwarded_call(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            15,
            CallTarget::Generic,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops ; .arch x64 ; add rsp, SPAN_BYTES);
        self.emit_call_completion(node)
    }

    pub(super) fn emit_call_completion(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let (threw, committed) = self.throw_targets(node);
        // A callee's Fatal is final: no handler may observe it and its parked
        // error must not be projected into an exception again.
        let fatal = self.fatal;
        let abrupt = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64 ; test rdx, rdx ; jnz =>abrupt ; =>done);
        // Every status but success leaves the hot path: a callee that retired
        // itself for a tail call it staged is entered in its place, anything
        // else is routed to its handler.
        self.defer(move |codegen| {
            codegen.emit_abrupt_completion(node, abrupt, done, (threw, committed, fatal))
        });
        Ok(())
    }

    /// The out-of-line part of a call's completion from `abrupt`, with a
    /// nonzero status in `rdx`: staged tail calls until one completes, then
    /// success back to `done` or the status's handler.
    fn emit_abrupt_completion(
        &mut self,
        node: NodeId,
        abrupt: DynamicLabel,
        done: DynamicLabel,
        (threw, committed, fatal): (DynamicLabel, DynamicLabel, DynamicLabel),
    ) -> Result<(), Unsupported> {
        let error = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; =>abrupt
            ; cmp edx, abi::NativeResultStatus::Continue as i32 ; jne =>error);
        let return_pc = crate::x86_64::js_call::emit_enter_staged(
            &mut self.ops,
            &mut self.relocations,
            self.transitions,
            15,
        );
        self.record_js_return(node, return_pc)?;
        dynasm!(self.ops ; .arch x64 ; test rdx, rdx ; jnz =>abrupt ; jmp =>done ; =>error);
        self.stamp_node_safepoint(node)?;
        self.stamp_pc(node);
        dynasm!(self.ops ; .arch x64
            ; cmp edx, abi::NativeResultStatus::Throw as i32 ; je =>committed
            ; cmp edx, abi::NativeResultStatus::Fatal as i32 ; je =>fatal
            ; jmp =>threw);
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
            identity_guard_exit: self.typed_exit(
                node,
                abi::ExitReason::IdentityGuard,
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
        // Values live across the operation sit in this body's tagged homes,
        // not in the register window, so an allocating stub call inside it
        // publishes the boundary's record over the homes — the record the
        // frame is stamped with — rather than the operation's window record.
        let alloc_roots = crate::template::operation::AllocRoots::Optimized {
            safepoint: self.helper_safepoint(node)?,
            tagged_slots: u16::try_from(self.slots.tagged)
                .map_err(|_| Unsupported::OperandShape("graph tagged home count"))?,
        };
        for position in positions {
            let operation = self.plan.instructions[position].op;
            self.stamp_node_safepoint(node)?;
            if !crate::template::operation_is_js_call(operation) {
                self.stamp_pc(node);
            }

            crate::template::x86_64::operation::emit_operation(
                crate::template::x86_64::operation::OperationContext {
                    ops: &mut self.ops,
                    relocations: &mut self.relocations,
                    return_sites: &mut self.return_sites,
                    call_safepoint,
                    alloc_roots,
                    transitions: self.transitions,
                    view: source,
                    plan: self.plan,
                    labels: &labels,
                    exits,
                    frame_kind: abi::NativeFrameKind::Optimizing,
                    shared_property: self.routines,
                    direct_call_events: &mut self.no_direct_call_events,
                    code_map: &mut self.no_code_map,
                },
                &self.plan.instructions[position],
            )?;
        }
        Ok(())
    }
}
