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
    pub(in crate::graph::x86_64) fn emit_lexical_allocation(
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
            size: 11,
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
                    crate::x86_64::allocation::emit_create_context(
                        &mut self.ops,
                        source,
                        plan,
                        15,
                        inputs[0],
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch x64 ; mov Rq(destination), Rq(regs.candidate) ; jmp =>done);
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
                crate::x86_64::allocation::emit_copy_context(
                    &mut self.ops,
                    source,
                    15,
                    inputs[0],
                    regs,
                    slow,
                );
                dynasm!(self.ops ; .arch x64 ; mov Rq(destination), Rq(regs.candidate) ; jmp =>done);
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
                    crate::x86_64::allocation::emit_closure(
                        &mut self.ops,
                        source,
                        plan,
                        15,
                        values,
                        regs,
                        slow,
                    );
                    dynasm!(self.ops ; .arch x64 ; mov Rq(destination), Rq(regs.candidate) ; jmp =>done);
                }
                let opcode = source.instructions[data.pc as usize].op(&source.code_block);
                let stub = match opcode {
                    otter_bytecode::Op::MakeFunction => abi::STUB_JIT_MAKE_FN,
                    otter_bytecode::Op::MakeClosure => abi::STUB_JIT_MAKE_CLOSURE,
                    _ => return Err(crate::Unsupported::OperandShape("closure source opcode")),
                };
                (stub, values)
            }
            Kind::NewArrayWithLength => (
                abi::STUB_ARRAY_CONSTRUCT_ALLOC,
                [inputs[0], undefined, undefined],
            ),
            _ => unreachable!("lexical allocation kind"),
        };
        dynasm!(self.ops ; .arch x64 ; =>slow);
        self.emit_probe_allocation(node, stub, values, Some(destination))?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        if owns_this {
            crate::x86_64::allocation::emit_publish_derived_this_context(
                &mut self.ops,
                14,
                destination,
                source.code_block.id,
                [regs.buffer, regs.scratch],
            );
        }
        Ok(())
    }

    pub(in crate::graph::x86_64) fn emit_probe_allocation(
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
        self.save_live_homes(node);
        let safepoint = self.stamp_node_safepoint(node)?;
        self.stamp_pc(node);
        dynasm!(self.ops ; .arch x64 ; sub rsp, bytes as i32);
        self.sp_delta += bytes;
        let values = values.map(|value| match value {
            AllocationValue::StackByte(byte) => AllocationValue::StackByte(byte + bytes),
            value => value,
        });
        dynasm!(self.ops ; .arch x64
            ; mov r10, [r15 + THREAD_OFFSET as i32] ; mov [rsp + ALLOC_CTX_THREAD_OFFSET as i32], r10
            ; mov r10, [r14 + abi::NATIVE_FRAME_MACHINE_ROOTS_OFFSET as i32] ; mov [rsp + ALLOC_CTX_SPILL_SLOTS_OFFSET as i32], r10
            ; mov DWORD [rsp + ALLOC_CTX_SAFEPOINT_ID_OFFSET as i32], safepoint as i32
            ; mov WORD [rsp + ALLOC_CTX_SPILL_SLOT_COUNT_OFFSET as i32], self.slots.tagged as i16
            ; mov rdi, rsp ; mov esi, safepoint as i32);

        for (register, value) in [2, 1, 8].into_iter().zip(values) {
            crate::x86_64::allocation::emit_value(&mut self.ops, register, value);
        }
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            entry as u64,
            descriptor,
        );
        crate::x86_64::call_abi::emit_runtime_call(&mut self.ops, descriptor);
        dynasm!(self.ops ; .arch x64 ; mov [rsp + ALLOC_CTX_STACK_SIZE as i32], rax ; mov [rsp + ALLOC_CTX_STACK_SIZE as i32 + 8], rdx);
        self.restore_live_homes(node);
        dynasm!(self.ops ; .arch x64 ; mov r10, [rsp + ALLOC_CTX_STACK_SIZE as i32] ; mov r11, [rsp + ALLOC_CTX_STACK_SIZE as i32 + 8] ; add rsp, bytes as i32);
        self.sp_delta -= bytes;
        let success = self.ops.new_dynamic_label();
        let fatal = self.fatal;
        dynasm!(self.ops ; .arch x64 ; test r11, r11 ; jz =>success
            ; cmp r11, abi::NativeResultStatus::SideExit as i32 ; je =>miss
            ; cmp r11, abi::NativeResultStatus::OutOfMemory as i32 ; je =>miss ; jmp =>fatal
            ; =>success);
        if let Some(destination) = destination {
            dynasm!(self.ops ; .arch x64 ; mov Rq(destination),r10);
        }
        Ok(())
    }
}
