//! Native lexical allocations with a canonical collecting Probe boundary.
//!
//! # Contents
//! - Exact-source context/closure plans and explicit boxed operand homes.
//! - LAB fit and fixed-three-value allocating cold calls.
//!
//! # Invariants
//! - The source view owns scope/function identity and encoded constant PC.
//! - The candidate is separate from the result and publication is complete.
//! - Call operands and all CFG-live values have mutable traced roots.
//! - GC-updated registers are restored before every Miss/OOM deopt; a cold
//!   exit must never spill stale pre-call registers over canonical homes.
//! - A Probe cannot throw/reenter; illegal statuses leave through fatal.
//!
//! # See also
//! - `crate::runtime_stubs` owns semantic allocation and root publication.
//! - `crate::allocation` owns the sole register/value recipe carriers.

use super::*;
use crate::entry::{
    ALLOC_CTX_SAFEPOINT_ID_OFFSET, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET, ALLOC_CTX_SPILL_SLOTS_OFFSET,
    ALLOC_CTX_STACK_SIZE, ALLOC_CTX_THREAD_OFFSET, THREAD_OFFSET,
};
use otter_vm::runtime_stubs::alloc_value_stub_by_id;

impl Codegen<'_> {
    pub(in crate::graph::arm64) fn emit_lexical_allocation(
        &mut self,
        node: NodeId,
        destination: u8,
    ) -> Result<(), crate::Unsupported> {
        let source = self.view_of(node);
        let data = self.graph.node(node);
        let kind = data.kind.clone();
        let owns_this = match kind {
            Kind::NativeNewContext(scope) => source
                .context_allocations
                .get(&(source.code_block.id, scope))
                .is_some_and(|plan| plan.derived_this_slot.is_some()),
            _ => false,
        };
        let byte_pc = source
            .instructions
            .get(data.pc as usize)
            .ok_or(crate::Unsupported::OperandShape(
                "lexical allocation source PC",
            ))?
            .byte_pc;
        let allocation = self.allocation.node(node);
        let inputs = allocation
            .inputs
            .iter()
            .map(|&location| match location {
                Location::TaggedSlot(_) => Ok(AllocationValue::StackByte(
                    self.slots.offset(location) + self.sp_delta,
                )),
                Location::Constant(value) => match self.graph.node(value).kind {
                    Kind::ConstTagged(bits) => Ok(AllocationValue::Constant(bits)),
                    _ => Err(crate::Unsupported::OperandShape(
                        "lexical input must be tagged",
                    )),
                },
                _ => Err(crate::Unsupported::OperandShape(
                    "lexical input needs a canonical home",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let regs = LabRegisters {
            buffer: allocation.gp_temps[0],
            candidate: allocation.gp_temps[1],
            end: allocation.gp_temps[2],
            scratch: allocation.gp_temps[3],
            size: 17,
        };
        debug_assert!(!allocation.gp_temps.contains(&destination));
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let undefined = AllocationValue::Constant(crate::entry::VALUE_UNDEFINED);
        let (stub, values) = match kind {
            Kind::NativeNewContext(scope) => {
                let function_id = i32::try_from(source.code_block.id)
                    .map_err(|_| crate::Unsupported::OperandShape("context function id"))?;
                let scope_word = i32::try_from(scope)
                    .map_err(|_| crate::Unsupported::OperandShape("context scope index"))?;
                if let Some(plan) = source
                    .context_allocations
                    .get(&(source.code_block.id, scope))
                {
                    crate::arm64::allocation::emit_create_context(
                        &mut self.ops,
                        source,
                        plan,
                        20,
                        inputs[0],
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(regs.candidate) ; b =>done);
                }
                (
                    abi::STUB_CREATE_CONTEXT_ALLOC,
                    [
                        inputs[0],
                        AllocationValue::Constant(
                            otter_vm::Value::number_i32(function_id).to_bits(),
                        ),
                        AllocationValue::Constant(
                            otter_vm::Value::number_i32(scope_word).to_bits(),
                        ),
                    ],
                )
            }
            Kind::CopyContext => {
                crate::arm64::allocation::emit_copy_context(
                    &mut self.ops,
                    source,
                    20,
                    inputs[0],
                    regs,
                    slow,
                );
                dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(regs.candidate) ; b =>done);
                (
                    abi::STUB_COPY_CONTEXT_ALLOC,
                    [inputs[0], undefined, undefined],
                )
            }
            Kind::NewClosure => {
                let values: [AllocationValue; 3] = inputs.try_into().map_err(|_| {
                    crate::Unsupported::OperandShape("closure needs context/this/new.target")
                })?;
                if let Some(&plan) = source.closure_allocations.get(&byte_pc) {
                    crate::arm64::allocation::emit_closure(
                        &mut self.ops,
                        source,
                        plan,
                        20,
                        values,
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(regs.candidate) ; b =>done);
                }
                let opcode = source.instructions[data.pc as usize].op(&source.code_block);
                let stub = match opcode {
                    otter_bytecode::Op::MakeFunction => abi::STUB_JIT_MAKE_FN,
                    otter_bytecode::Op::MakeClosure => abi::STUB_JIT_MAKE_CLOSURE,
                    _ => return Err(crate::Unsupported::OperandShape("closure source opcode")),
                };
                (stub, values)
            }
            _ => unreachable!("lexical allocation kind"),
        };
        dynasm!(self.ops ; .arch aarch64 ; =>slow);
        self.emit_probe_allocation(node, stub, values, Some(destination))?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        if owns_this {
            crate::arm64::allocation::emit_publish_derived_this_context(
                &mut self.ops,
                21,
                destination,
                source.code_block.id,
                [regs.buffer, regs.scratch],
            );
        }
        Ok(())
    }

    pub(in crate::graph::arm64) fn emit_probe_allocation(
        &mut self,
        node: NodeId,
        descriptor: abi::RuntimeStubDescriptor,
        values: [AllocationValue; 3],
        destination: Option<u8>,
    ) -> Result<(), crate::Unsupported> {
        assert_eq!(descriptor.signature, abi::RuntimeStubSignature::AllocValue3);
        assert_eq!(descriptor.result_domain, abi::NativeResultDomain::Probe);
        let entry = alloc_value_stub_by_id(descriptor.id)
            .and_then(|stub| stub.entry_addr())
            .ok_or(crate::Unsupported::OperandShape(
                "typed lexical allocation entry",
            ))?;
        let miss = self.typed_exit(
            node,
            abi::ExitReason::AllocationMiss,
            abi::ExitAction::Resume,
        );
        let bytes = ALLOC_CTX_STACK_SIZE + 16;
        let live = self.allocation.node(node).live_homes.clone();
        for &(location, home) in &live {
            if !matches!(home, Location::Constant(_)) {
                self.emit_move(location, home);
            }
        }
        let safepoint = self.stamp_node_safepoint(node)?;
        self.load_word32(16, self.graph.outer_pc(node));
        dynasm!(self.ops ; .arch aarch64 ; str w16, [x21, crate::entry::NATIVE_FRAME_PC_OFFSET]);
        dynasm!(self.ops ; .arch aarch64 ; sub sp, sp, bytes);
        self.sp_delta += bytes;
        let values = values.map(|value| match value {
            AllocationValue::StackByte(byte) => AllocationValue::StackByte(byte + bytes),
            value => value,
        });
        self.load_word32(1, safepoint);
        self.load_word32(9, self.slots.tagged);
        dynasm!(self.ops ; .arch aarch64
            ; ldr x10, [x20, THREAD_OFFSET] ; str x10, [sp, ALLOC_CTX_THREAD_OFFSET]
            ; ldr x10, [x21, abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET] ; str x10, [sp, ALLOC_CTX_SPILL_SLOTS_OFFSET]
            ; str w1, [sp, ALLOC_CTX_SAFEPOINT_ID_OFFSET] ; strh w9, [sp, ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET] ; mov x0, sp);

        for (register, value) in [2, 3, 4].into_iter().zip(values) {
            crate::arm64::allocation::emit_value(&mut self.ops, register, value);
        }
        crate::template::arm64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            16,
            entry as u64,
            descriptor,
        );
        dynasm!(self.ops ; .arch aarch64 ; blr x16 ; stp x0, x1, [sp, ALLOC_CTX_STACK_SIZE as i32]);
        for &(location, home) in &live {
            self.emit_move(home, location);
        }
        dynasm!(self.ops ; .arch aarch64 ; ldp x16, x17, [sp, ALLOC_CTX_STACK_SIZE as i32] ; add sp, sp, bytes);
        self.sp_delta -= bytes;
        let success = self.ops.new_dynamic_label();
        let miss = self.cond_target(miss);
        let fatal = self.cond_target(self.fatal);
        dynasm!(self.ops ; .arch aarch64 ; cbz x17, =>success
            ; cmp x17, abi::NativeResultStatus::SideExit as u32 ; b.eq =>miss
            ; cmp x17, abi::NativeResultStatus::OutOfMemory as u32 ; b.eq =>miss ; b =>fatal
            ; =>success);
        if let Some(destination) = destination {
            dynasm!(self.ops ; .arch aarch64 ; mov X(destination),x16);
        }
        Ok(())
    }
}
