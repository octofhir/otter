//! Primitive Number/String operations over canonical Graph operands.
//!
//! # Contents
//! - Native string LAB fits and Number addition on one source operation.
//! - BigInt binary operators through their allocating Probe.
//! - Typed UTF-16/numeric ordering through the VM's NoAlloc boundary.
//!
//! # Invariants
//! Add inputs stay in canonical traced homes, and cold Probe restoration precedes
//! every eager exit. Neither content ordering nor native fits can collect or
//! reenter. No character pointer escapes the node. Unsupported values leave
//! before observable coercion at the exact source instruction.
//!
//! # See also
//! - `super::lexical` owns the sole collecting fixed-value Probe packet.
//! - `otter_vm::runtime_stubs` owns primitive conversion and comparison.

use super::super::Condition;
use super::*;
use crate::entry::VM_THREAD_GC_HEAP_OFFSET;
use crate::template::arm64::values::emit_load_symbol_u64;

impl Codegen<'_> {
    fn primitive_inputs(&self, node: NodeId) -> Result<[AllocationValue; 2], crate::Unsupported> {
        let mut values = Vec::new();
        for &location in &self.allocation.node(node).inputs {
            values.push(match location {
                Location::TaggedSlot(_) => {
                    AllocationValue::StackByte(self.slots.offset(location) + self.sp_delta)
                }
                Location::Constant(value) => match self.graph.node(value).kind {
                    Kind::ConstTagged(bits) => AllocationValue::Constant(bits),
                    _ => {
                        return Err(crate::Unsupported::OperandShape(
                            "primitive input must be tagged",
                        ));
                    }
                },
                _ => {
                    return Err(crate::Unsupported::OperandShape(
                        "primitive add needs canonical homes",
                    ));
                }
            });
        }
        values
            .try_into()
            .map_err(|_| crate::Unsupported::OperandShape("primitive add arity"))
    }

    pub(in crate::graph::arm64) fn emit_primitive_add(
        &mut self,
        node: NodeId,
    ) -> Result<(), crate::Unsupported> {
        let values = self.primitive_inputs(node)?;
        let a = self.allocation.node(node);
        let destination = Self::gp(a.result.expect("primitive result"));
        let regs = LabRegisters {
            buffer: a.gp_temps[0],
            candidate: a.gp_temps[1],
            end: a.gp_temps[2],
            scratch: a.gp_temps[3],
            size: 17,
        };
        let float = a.fp_temps[0];
        let strings = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        crate::arm64::allocation::emit_value(&mut self.ops, regs.buffer, values[0]);
        crate::arm64::allocation::emit_value(&mut self.ops, regs.end, values[1]);
        self.emit_tagged_to_float(regs.buffer, float, strings);
        self.emit_tagged_to_float(regs.end, 31, strings);
        dynasm!(self.ops ; .arch aarch64 ; fadd D(float),D(float),d31);
        self.emit_box_float64(float, destination);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>strings);
        let layout = self.view_of(node).string_layout;
        crate::arm64::allocation::emit_concat(&mut self.ops, 20, layout, values, regs, slow);
        dynasm!(self.ops ; .arch aarch64 ; mov X(destination),X(regs.candidate) ; b =>done ; =>slow);
        self.emit_probe_allocation(
            node,
            abi::STUB_STRING_CONCAT_ALLOC,
            [
                values[0],
                values[1],
                AllocationValue::Constant(crate::entry::VALUE_UNDEFINED),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }
    /// A BigInt binary operator through the allocating Probe; a non-BigInt
    /// operand or an operator failure exits to the source instruction.
    pub(in crate::graph::arm64) fn emit_bigint_binary(
        &mut self,
        node: NodeId,
        operator: otter_vm::bigint::ops::Operator,
    ) -> Result<(), crate::Unsupported> {
        let [lhs, rhs] = self.primitive_inputs(node)?;
        let destination = Self::gp(self.allocation.node(node).result.expect("bigint result"));
        let code = otter_vm::Value::number_i32(i32::from(operator as u8)).to_bits();
        self.emit_probe_allocation(
            node,
            abi::STUB_BIGINT_BINARY_ALLOC,
            [lhs, rhs, AllocationValue::Constant(code)],
            Some(destination),
        )
    }
    pub(in crate::graph::arm64) fn emit_primitive_compare(
        &mut self,
        node: NodeId,
        condition: Condition,
    ) -> Result<(), crate::Unsupported> {
        let loc = self.allocation.node(node);
        let a = Self::gp(loc.inputs[0]);
        let b = Self::gp(loc.inputs[1]);
        let destination = Self::gp(loc.result.expect("primitive order result"));
        let live = loc.live_registers.clone();
        let saved = self.emit_save_registers(&live);
        dynasm!(self.ops ; .arch aarch64 ; mov x16,X(a) ; mov x17,X(b) ; mov x1,x16 ; mov x2,x17
            ; ldr x0,[x20,crate::entry::THREAD_OFFSET] ; ldr x0,[x0,VM_THREAD_GC_HEAP_OFFSET]);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            16,
            otter_vm::runtime_stubs::PRIMITIVE_STRING_ORDER.entry_addr() as u64,
            crate::artifact::relocation::RelocationTarget::runtime_stub(
                abi::STUB_PRIMITIVE_STRING_ORDER,
            ),
        );
        dynasm!(self.ops ; .arch aarch64 ; blr x16 ; mov x16,x0 ; mov x17,x1);
        self.emit_restore_registers(&live, saved);
        let success = self.ops.new_dynamic_label();
        let miss = self.eager_exit(node, super::super::DeoptReason::WrongType);
        let miss = self.cond_target(miss);
        let fatal = self.cond_target(self.fatal);
        dynasm!(self.ops ; .arch aarch64 ; cbz x17,=>success
            ; cmp x17,#abi::NativeResultStatus::SideExit as u32 ; b.eq =>miss ; b =>fatal ; =>success);
        // Order is -1/0/1, with +2 for unordered. The latter satisfies only !=.
        match condition {
            Condition::Equal => {
                dynasm!(self.ops ; .arch aarch64 ; cmp w16,#0 ; cset W(destination),eq)
            }
            Condition::NotEqual => {
                dynasm!(self.ops ; .arch aarch64 ; cmp w16,#0 ; cset W(destination),ne)
            }
            Condition::Less => {
                dynasm!(self.ops ; .arch aarch64 ; cmp w16,#0 ; cset W(destination),lt)
            }
            Condition::LessEqual => {
                dynasm!(self.ops ; .arch aarch64 ; cmp w16,#0 ; cset W(destination),le)
            }
            Condition::Greater => {
                dynasm!(self.ops ; .arch aarch64 ; cmp w16,#1 ; cset W(destination),eq)
            }
            Condition::GreaterEqual => {
                dynasm!(self.ops ; .arch aarch64 ; cmp w16,#1 ; cset W(destination),ls)
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; orr XSP(destination),X(destination),crate::entry::VALUE_FALSE);
        Ok(())
    }
}
