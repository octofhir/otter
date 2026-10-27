//! Primitive Number/String operations over canonical Graph operands.
//!
//! # Contents
//! - Native string LAB fits and Number addition on one source operation,
//!   placed by the `+` instruction's allocation site.
//! - A Number's decimal string read from the isolate's cache.
//! - BigInt binary operators through their allocating Probe.
//! - Typed UTF-16/numeric ordering through the VM's NoAlloc boundary.
//!
//! # Invariants
//! Add inputs stay in canonical traced homes, and cold Probe restoration precedes
//! every eager exit. Neither content ordering nor native fits can collect or
//! reenter. No character pointer escapes the node. Unsupported values leave
//! before observable coercion at the exact source instruction.
//! A tracking site leaves before any effect once it tenures; one the
//! collector declined keeps tagging cells, which only its counters see.
//! An old site's cons takes the write barrier for both children itself, so
//! a young or white child never sends it to the collecting Probe.
//!
//! # See also
//! - `super::lexical` owns the sole collecting fixed-value Probe packet.
//! - `otter_vm::runtime_stubs` owns primitive conversion and comparison.

use super::super::Condition;
use super::*;
use crate::allocation::ConcatPlacement;
use crate::entry::VM_THREAD_GC_HEAP_OFFSET;
use crate::template::arm64::values::emit_load_symbol_u64;
use otter_vm::jit::JIT_SITE_TENURE;

impl Codegen<'_> {
    fn primitive_inputs(&self, node: NodeId) -> Result<[AllocationValue; 2], crate::Unsupported> {
        self.primitive_operands::<2>(node)
    }

    fn primitive_operands<const N: usize>(
        &self,
        node: NodeId,
    ) -> Result<[AllocationValue; N], crate::Unsupported> {
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

    /// `ToString` of a Number: a small integer's string by value, any other
    /// Number's by its bits' entry; an empty or other entry reaches the
    /// collecting Probe, which converts and fills it.
    pub(in crate::graph::arm64) fn emit_number_to_string(
        &mut self,
        node: NodeId,
    ) -> Result<(), crate::Unsupported> {
        use otter_vm::string::number_strings as table;
        let [value] = self.primitive_operands::<1>(node)?;
        let a = self.allocation.node(node);
        let destination = Self::gp(a.result.expect("number string result"));
        let (word, strings) = (a.gp_temps[0], a.gp_temps[1]);
        let float = a.fp_temps[0];
        let slow = self.ops.new_dynamic_label();
        let double = self.ops.new_dynamic_label();
        let found = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        crate::arm64::allocation::emit_value(&mut self.ops, word, value);
        dynasm!(self.ops ; .arch aarch64
            ; ldr X(strings), [x20, crate::entry::THREAD_OFFSET]
            ; ldr X(strings), [X(strings), crate::entry::VM_THREAD_NUMBER_STRINGS_OFFSET]
            ; cbz X(strings), =>slow
            ; asr x16, X(word), super::super::INT32_TAG_SHIFT
            ; cmn x16, 1
            ; b.ne =>double
            ; mov w16, W(word)
            ; cmp w16, table::NUMBER_STRINGS_SMALL as u32
            ; b.hs =>double
            ; ldr x16, [X(strings), x16, lsl 3]
            ; b =>found
            ; =>double
        );
        self.emit_tagged_to_float(word, float, slow);
        dynasm!(self.ops ; .arch aarch64
            ; fmov x16, D(float)
            ; eor x17, x16, x16, lsr 32
            ; and x17, x17, (table::NUMBER_STRINGS_ENTRIES - 1) as u64
            ; add x17, X(strings), x17, lsl table::NUMBER_STRING_ENTRY_SHIFT
            ; ldr X(word), [x17, table::NUMBER_STRINGS_ENTRIES_BYTE as u32]
            ; cmp X(word), x16
            ; b.ne =>slow
            ; ldr x16, [x17, (table::NUMBER_STRINGS_ENTRIES_BYTE
                + table::NUMBER_STRING_ENTRY_STRING_BYTE) as u32]
            ; =>found
        );
        self.load_immediate(17, crate::entry::VALUE_UNDEFINED);
        dynasm!(self.ops ; .arch aarch64
            ; cmp x16, x17
            ; b.eq =>slow
            ; mov X(destination), x16
            ; b =>done
            ; =>slow
        );
        let undefined = AllocationValue::Constant(crate::entry::VALUE_UNDEFINED);
        self.emit_probe_allocation(
            node,
            abi::STUB_NUMBER_TO_STRING_ALLOC,
            [value, undefined, undefined],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
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
        let source = self.view_of(node);
        let layout = source.string_layout;
        let site = source
            .literal_allocations
            .sites
            .get(&self.graph.node(node).pc)
            .copied()
            .unwrap_or_default();
        if site.mode == JitSiteMode::Track {
            // A tenured site leaves before any effect and recompiles old.
            let exit = self.eager_exit(node, DeoptReason::AllocationSiteDecided);
            self.load_immediate(16, site.record);
            dynasm!(self.ops ; .arch aarch64
                ; ldrb w17, [x16, JIT_SITE_DECISION_OFFSET]
                ; cmp w17, u32::from(JIT_SITE_TENURE) ; b.eq =>exit);
        }
        let old_cons = crate::arm64::allocation::emit_concat(
            &mut self.ops,
            20,
            layout,
            values,
            regs,
            ConcatPlacement::Site(site),
            slow,
        );
        dynasm!(self.ops ; .arch aarch64 ; mov X(destination),X(regs.candidate) ; b =>done);
        if let Some(old_cons) = old_cons {
            // A pretenured cons may hold a young child, or a white one while
            // marking runs: both edges take the barrier before anything runs.
            dynasm!(self.ops ; .arch aarch64 ; =>old_cons ; mov X(destination),X(regs.candidate));
            crate::arm64::allocation::emit_value(&mut self.ops, regs.buffer, values[0]);
            crate::arm64::allocation::emit_value(&mut self.ops, regs.end, values[1]);
            self.emit_write_barrier_keeping(
                node,
                destination,
                regs.buffer,
                &[destination, regs.end],
            );
            self.emit_write_barrier_keeping(node, destination, regs.end, &[destination]);
            dynasm!(self.ops ; .arch aarch64 ; b =>done);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>slow);
        self.emit_probe_allocation(
            node,
            abi::STUB_STRING_CONCAT_ALLOC,
            [
                values[0],
                values[1],
                AllocationValue::Constant(ConcatPlacement::stub_operand(site)),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }
    /// A BigInt binary operator. Operands that fit `i64` take the inline
    /// BigInt64 path, whose result is carved in the buffer; every other
    /// case, and any overflow, reaches the allocating Probe, which exits to
    /// the source instruction for a non-BigInt operand or a throwing operator.
    pub(in crate::graph::arm64) fn emit_bigint_binary(
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
        if !matches!(operator, Operator::Shl | Operator::Shr) {
            crate::arm64::allocation::emit_value(&mut self.ops, x, lhs);
            crate::arm64::allocation::emit_value(&mut self.ops, y, rhs);
            crate::arm64::allocation::emit_unbox_bigint64(&mut self.ops, x, 16, slow);
            crate::arm64::allocation::emit_unbox_bigint64(&mut self.ops, y, 16, slow);
            match operator {
                Operator::Add => dynasm!(self.ops ; .arch aarch64
                    ; adds X(result), X(x), X(y) ; b.vs =>slow),
                Operator::Sub => dynasm!(self.ops ; .arch aarch64
                    ; subs X(result), X(x), X(y) ; b.vs =>slow),
                Operator::Mul => dynasm!(self.ops ; .arch aarch64
                    ; mul X(result), X(x), X(y) ; smulh x16, X(x), X(y)
                    ; cmp x16, X(result), asr 63 ; b.ne =>slow),
                // Zero divisors throw and `i64::MIN / -1` leaves `i64`.
                Operator::Div => dynasm!(self.ops ; .arch aarch64
                    ; cbz X(y), =>slow ; cmn XSP(y), 1 ; b.ne >divide
                    ; negs x16, X(x) ; b.vs =>slow
                    ; divide: ; sdiv X(result), X(x), X(y)),
                Operator::Rem => dynasm!(self.ops ; .arch aarch64
                    ; cbz X(y), =>slow ; sdiv x16, X(x), X(y)
                    ; msub X(result), x16, X(y), X(x)),
                Operator::BitwiseAnd => {
                    dynasm!(self.ops ; .arch aarch64 ; and X(result), X(x), X(y))
                }
                Operator::BitwiseOr => {
                    dynasm!(self.ops ; .arch aarch64 ; orr X(result), X(x), X(y))
                }
                Operator::BitwiseXor => {
                    dynasm!(self.ops ; .arch aarch64 ; eor X(result), X(x), X(y))
                }
                Operator::Shl | Operator::Shr => unreachable!("shifts take the Probe"),
            }
            let regs = LabRegisters {
                buffer: x,
                candidate,
                end: y,
                scratch,
                size: 17,
            };
            crate::arm64::allocation::emit_bigint64(&mut self.ops, 20, result, 16, regs, slow);
            dynasm!(self.ops ; .arch aarch64 ; mov X(destination), X(candidate) ; b =>done);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>slow);
        let code = otter_vm::Value::number_i32(i32::from(operator as u8)).to_bits();
        self.emit_probe_allocation(
            node,
            abi::STUB_BIGINT_BINARY_ALLOC,
            [lhs, rhs, AllocationValue::Constant(code)],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
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
