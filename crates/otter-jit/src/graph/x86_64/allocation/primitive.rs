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

    pub(in crate::graph::x86_64) fn emit_primitive_add(
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
            size: 11,
        };
        let float = a.fp_temps[0];
        let strings = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        crate::x86_64::allocation::emit_value(&mut self.ops, regs.buffer, values[0]);
        crate::x86_64::allocation::emit_value(&mut self.ops, regs.end, values[1]);
        super::super::scalar::conversions::emit_tagged_number_to_float(
            &mut self.ops,
            regs.buffer,
            float,
            strings,
        );
        super::super::scalar::conversions::emit_tagged_number_to_float(
            &mut self.ops,
            regs.end,
            15,
            strings,
        );
        dynasm!(self.ops ; .arch x64 ; addsd Rx(float),xmm15);
        self.emit_box_float64(node, destination, float);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>strings);
        let layout = self.view_of(node).string_layout;
        crate::x86_64::allocation::emit_concat(&mut self.ops, 15, layout, values, regs, slow);
        dynasm!(self.ops ; .arch x64 ; mov Rq(destination),Rq(regs.candidate) ; jmp =>done ; =>slow);
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
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }
    /// A BigInt binary operator. Operands that fit `i64` take the inline
    /// BigInt64 path (V8's), whose result is carved in the buffer; every other
    /// case, and any overflow, reaches the allocating Probe, which exits to
    /// the source instruction for a non-BigInt operand or a throwing operator.
    /// Division and remainder keep the Probe: `idiv` owns `rax`/`rdx`.
    pub(in crate::graph::x86_64) fn emit_bigint_binary(
        &mut self,
        node: NodeId,
        operator: otter_vm::bigint::ops::Operator,
    ) -> Result<(), crate::Unsupported> {
        use otter_vm::bigint::ops::Operator;
        let [lhs, rhs] = self.primitive_inputs(node)?;
        let a = self.allocation.node(node);
        let destination = Self::gp(a.result.expect("bigint result"));
        let [x, y, result, candidate, scratch] = [
            a.gp_temps[0],
            a.gp_temps[1],
            a.gp_temps[2],
            a.gp_temps[3],
            a.gp_temps[4],
        ];
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let inline = !matches!(
            operator,
            Operator::Div | Operator::Rem | Operator::Shl | Operator::Shr
        );
        if inline {
            crate::x86_64::allocation::emit_value(&mut self.ops, x, lhs);
            crate::x86_64::allocation::emit_value(&mut self.ops, y, rhs);
            crate::x86_64::allocation::emit_unbox_bigint64(&mut self.ops, x, 10, slow);
            crate::x86_64::allocation::emit_unbox_bigint64(&mut self.ops, y, 10, slow);
            dynasm!(self.ops ; .arch x64 ; mov Rq(result), Rq(x));
            match operator {
                Operator::Add => dynasm!(self.ops ; .arch x64 ; add Rq(result), Rq(y) ; jo =>slow),
                Operator::Sub => dynasm!(self.ops ; .arch x64 ; sub Rq(result), Rq(y) ; jo =>slow),
                Operator::Mul => dynasm!(self.ops ; .arch x64 ; imul Rq(result), Rq(y) ; jo =>slow),
                Operator::BitwiseAnd => dynasm!(self.ops ; .arch x64 ; and Rq(result), Rq(y)),
                Operator::BitwiseOr => dynasm!(self.ops ; .arch x64 ; or Rq(result), Rq(y)),
                Operator::BitwiseXor => dynasm!(self.ops ; .arch x64 ; xor Rq(result), Rq(y)),
                Operator::Div | Operator::Rem | Operator::Shl | Operator::Shr => {
                    unreachable!("these operators take the Probe")
                }
            }
            let regs = LabRegisters {
                buffer: x,
                candidate,
                end: y,
                scratch,
                size: 11,
            };
            crate::x86_64::allocation::emit_bigint64(&mut self.ops, 15, result, 10, regs, slow);
            dynasm!(self.ops ; .arch x64 ; mov Rq(destination), Rq(candidate) ; jmp =>done);
        }
        dynasm!(self.ops ; .arch x64 ; =>slow);
        let code = otter_vm::Value::number_i32(i32::from(operator as u8)).to_bits();
        self.emit_probe_allocation(
            node,
            abi::STUB_BIGINT_BINARY_ALLOC,
            [lhs, rhs, AllocationValue::Constant(code)],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }
    pub(in crate::graph::x86_64) fn emit_primitive_compare(
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
        dynasm!(self.ops ; .arch x64 ; mov r10,Rq(a) ; mov r11,Rq(b) ; mov rsi,r10 ; mov rdx,r11
            ; mov rdi,[r15+crate::entry::THREAD_OFFSET as i32] ; mov rdi,[rdi+VM_THREAD_GC_HEAP_OFFSET as i32]);
        self.emit_scalar_vm_leaf(
            abi::STUB_PRIMITIVE_STRING_ORDER,
            otter_vm::runtime_stubs::PRIMITIVE_STRING_ORDER.entry_addr() as u64,
        );
        dynasm!(self.ops ; .arch x64 ; mov r10,rax ; mov r11,rdx);
        self.emit_restore_registers(&live, saved);
        let success = self.ops.new_dynamic_label();
        let miss = self.eager_exit(node, super::super::DeoptReason::WrongType);
        let fatal = self.fatal;
        dynasm!(self.ops ; .arch x64 ; test r11,r11 ; jz =>success
            ; cmp r11,abi::NativeResultStatus::SideExit as i32 ; je =>miss ; jmp =>fatal ; =>success);
        // The VM result is tagged Int32; compare only its signed payload.
        match condition {
            Condition::Equal => dynasm!(self.ops ; .arch x64 ; cmp r10d,0 ; sete r11b),
            Condition::NotEqual => dynasm!(self.ops ; .arch x64 ; cmp r10d,0 ; setne r11b),
            Condition::Less => dynasm!(self.ops ; .arch x64 ; cmp r10d,0 ; setl r11b),
            Condition::LessEqual => dynasm!(self.ops ; .arch x64 ; cmp r10d,0 ; setle r11b),
            Condition::Greater => dynasm!(self.ops ; .arch x64 ; cmp r10d,1 ; sete r11b),
            Condition::GreaterEqual => dynasm!(self.ops ; .arch x64 ; cmp r10d,1 ; setbe r11b),
        }
        dynasm!(self.ops ; .arch x64 ; movzx Rd(destination),r11b ; or Rq(destination),crate::entry::VALUE_FALSE as i32);
        Ok(())
    }
}
