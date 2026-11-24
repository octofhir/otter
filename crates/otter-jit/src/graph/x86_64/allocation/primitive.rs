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
    /// A registry builtin's allocating entry on the node's three operand
    /// words; a miss resumes the call it reduced. `slice` and `substring`
    /// of a sequential Latin-1 string with Int32 or absent bounds and a short
    /// result carve it from the nursery buffer first.
    pub(in crate::graph::x86_64) fn emit_native_alloc_leaf(
        &mut self,
        node: NodeId,
        stub: abi::RuntimeStubId,
    ) -> Result<(), crate::Unsupported> {
        let descriptor = otter_vm::runtime_stubs::alloc_value_stub_by_id(stub)
            .ok_or(crate::Unsupported::OperandShape(
                "graph allocating builtin entry",
            ))?
            .descriptor;
        let values = self.primitive_operands::<3>(node)?;
        let destination = Self::gp(
            self.allocation
                .node(node)
                .result
                .expect("allocating builtin result"),
        );
        let slice = stub == abi::STUB_STRING_SLICE_ALLOC.id;
        if !slice && stub != abi::STUB_STRING_SUBSTRING_ALLOC.id {
            return self.emit_probe_allocation(node, descriptor, values, Some(destination));
        }
        let a = self.allocation.node(node);
        let regs = LabRegisters {
            buffer: a.gp_temps[0],
            candidate: a.gp_temps[1],
            end: a.gp_temps[2],
            scratch: a.gp_temps[3],
            size: 11,
        };
        let (from, count) = (a.gp_temps[4], a.gp_temps[5]);
        let p = self.view_of(node).string_layout;
        let slow = self.ops.new_dynamic_label();
        let unit = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let [source, start, end] = values;
        let len = regs.scratch;
        crate::x86_64::allocation::emit_value(&mut self.ops, regs.buffer, source);
        crate::x86_64::values::emit_load_u64(
            &mut self.ops,
            10,
            otter_vm::value::tag::NOT_CELL_MASK,
        );
        dynasm!(self.ops ; .arch x64
            ; test Rq(regs.buffer), r10 ; jnz =>slow
            ; test Rq(regs.buffer), Rq(regs.buffer) ; jz =>slow
            ; cmp BYTE [Rq(regs.buffer)], p.string_type_tag as i8 ; jne =>slow
            ; cmp BYTE [Rq(regs.buffer) + p.string_repr_byte as i32], p.seq_latin1_tag as i8
            ; jne =>slow
            ; mov Rd(len), [Rq(regs.buffer) + p.string_len_byte as i32]);
        // Each bound: absent is its default, an Int32 itself, anything else
        // takes the entry.
        for (register, value, default) in [(from, start, None), (count, end, Some(len))] {
            let absent = self.ops.new_dynamic_label();
            let read = self.ops.new_dynamic_label();
            crate::x86_64::allocation::emit_value(&mut self.ops, register, value);
            crate::x86_64::values::emit_load_u64(&mut self.ops, 10, crate::entry::VALUE_UNDEFINED);
            dynasm!(self.ops ; .arch x64
                ; cmp Rq(register), r10 ; je =>absent
                ; mov r10, Rq(register) ; sar r10, 49 ; cmp r10, -1 ; jne =>slow
                ; movsxd Rq(register), Rd(register) ; jmp =>read ; =>absent);
            match default {
                None => dynasm!(self.ops ; .arch x64 ; xor Rd(register), Rd(register)),
                Some(len) => dynasm!(self.ops ; .arch x64 ; mov Rd(register), Rd(len)),
            }
            dynasm!(self.ops ; .arch x64 ; =>read);
        }
        if slice {
            // A negative bound counts from the end; the range is never reversed.
            for register in [from, count] {
                dynasm!(self.ops ; .arch x64
                    ; test Rq(register), Rq(register) ; jns >positive
                    ; add Rq(register), Rq(len)
                    ; xor r10d, r10d ; cmp Rq(register), r10 ; cmovl Rq(register), r10
                    ; jmp >bounded
                    ; positive:
                    ; cmp Rq(register), Rq(len) ; cmovg Rq(register), Rq(len)
                    ; bounded:);
            }
            dynasm!(self.ops ; .arch x64
                ; sub Rq(count), Rq(from)
                ; xor r10d, r10d ; cmp Rq(count), r10 ; cmovl Rq(count), r10);
        } else {
            // Both bounds clamp to the string; a reversed pair swaps.
            for register in [from, count] {
                dynasm!(self.ops ; .arch x64
                    ; xor r10d, r10d ; cmp Rq(register), r10 ; cmovl Rq(register), r10
                    ; cmp Rq(register), Rq(len) ; cmovg Rq(register), Rq(len));
            }
            dynasm!(self.ops ; .arch x64
                ; mov r10, Rq(from)
                ; cmp Rq(from), Rq(count) ; cmovg Rq(from), Rq(count) ; cmovg Rq(count), r10
                ; sub Rq(count), Rq(from));
        }
        // A one-unit result is already rooted by the isolate's Latin-1
        // table. Preserve the source for the collecting miss.
        dynasm!(self.ops ; .arch x64
            ; cmp Rq(count), 1 ; jb =>slow ; je =>unit
            ; cmp Rq(count), i32::from(p.short_concat_latin1) ; ja =>slow);
        crate::x86_64::allocation::emit_substring_latin1(
            &mut self.ops,
            15,
            p,
            source,
            [from, count],
            regs,
            slow,
        );
        dynasm!(self.ops ; .arch x64
            ; mov Rq(destination), Rq(regs.candidate) ; jmp =>done
            ; =>unit
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_UNIT_STRINGS_OFFSET as i32]
            ; test r10, r10 ; jz =>slow
            ; movzx r11d, BYTE [Rq(regs.buffer) + Rq(from) + p.string_body_size as i32]
            ; mov Rq(destination), [r10 + r11 * 8] ; jmp =>done
            ; =>slow);
        self.emit_probe_allocation(node, descriptor, values, Some(destination))?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    pub(in crate::graph::x86_64) fn emit_number_to_string(
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
        crate::x86_64::allocation::emit_value(&mut self.ops, word, value);
        dynasm!(self.ops ; .arch x64
            ; mov Rq(strings), [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov Rq(strings), [Rq(strings) + crate::entry::VM_THREAD_NUMBER_STRINGS_OFFSET as i32]
            ; test Rq(strings), Rq(strings)
            ; jz =>slow
            ; mov r10, Rq(word) ; sar r10, 49 ; cmp r10, -1 ; jne =>double
            ; mov r10d, Rd(word)
            ; cmp r10d, table::NUMBER_STRINGS_SMALL as i32
            ; jae =>double
            ; mov r10, [Rq(strings) + r10 * 8]
            ; jmp =>found
            ; =>double
        );
        super::super::scalar::conversions::emit_tagged_number_to_float(
            &mut self.ops,
            word,
            float,
            slow,
        );
        dynasm!(self.ops ; .arch x64
            ; movq r10, Rx(float)
            ; mov r11, r10 ; shr r11, 32 ; xor r11, r10
            ; and r11d, (table::NUMBER_STRINGS_ENTRIES - 1) as i32
            ; shl r11, table::NUMBER_STRING_ENTRY_SHIFT as i8
            ; add r11, Rq(strings)
            ; cmp r10, [r11 + table::NUMBER_STRINGS_ENTRIES_BYTE as i32]
            ; jne =>slow
            ; mov r10, [r11 + (table::NUMBER_STRINGS_ENTRIES_BYTE
                + table::NUMBER_STRING_ENTRY_STRING_BYTE) as i32]
            ; =>found
        );
        self.load_immediate(11, crate::entry::VALUE_UNDEFINED);
        dynasm!(self.ops ; .arch x64
            ; cmp r10, r11
            ; je =>slow
            ; mov Rq(destination), r10
            ; jmp =>done
            ; =>slow
        );
        let undefined = AllocationValue::Constant(crate::entry::VALUE_UNDEFINED);
        self.emit_probe_allocation(
            node,
            abi::STUB_NUMBER_TO_STRING_ALLOC,
            [value, undefined, undefined],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    pub(in crate::graph::x86_64) fn emit_primitive_add(
        &mut self,
        node: NodeId,
        input_strings: [bool; 2],
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
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        // If a proven String participates, this source add cannot be numeric.
        if !input_strings[0] && !input_strings[1] {
            let float = a.fp_temps[0];
            let strings = self.ops.new_dynamic_label();
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
        }

        let cacheable = input_strings != [false; 2];
        let initialized = if cacheable {
            self.ops.new_dynamic_label()
        } else {
            done
        };
        if cacheable {
            super::concat_cache::emit_hit(&mut self.ops, values, regs, destination, done);
        }
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
            self.load_immediate(10, site.record);
            dynasm!(self.ops ; .arch x64
                ; cmp BYTE [r10 + JIT_SITE_DECISION_OFFSET as i32], JIT_SITE_TENURE as i8
                ; je =>exit);
        }
        let old_cons = crate::x86_64::allocation::emit_concat(
            &mut self.ops,
            15,
            layout,
            crate::allocation::ConcatInputs {
                values,
                input_strings,
            },
            regs,
            ConcatPlacement::Site(site),
            slow,
        );
        dynasm!(self.ops ; .arch x64 ; mov Rq(destination),Rq(regs.candidate) ; jmp =>initialized);
        if let Some(old_cons) = old_cons {
            // A pretenured cons may hold a young child, or a white one while
            // marking runs: both edges take the barrier before anything runs.
            dynasm!(self.ops ; .arch x64 ; =>old_cons ; mov Rq(destination),Rq(regs.candidate));
            crate::x86_64::allocation::emit_value(&mut self.ops, regs.buffer, values[0]);
            crate::x86_64::allocation::emit_value(&mut self.ops, regs.end, values[1]);
            self.emit_cell_edge_barrier(node, destination, regs.buffer, &[destination, regs.end]);
            self.emit_cell_edge_barrier(node, destination, regs.end, &[destination]);
            dynasm!(self.ops ; .arch x64 ; jmp =>initialized);
        }

        if cacheable {
            dynasm!(self.ops ; .arch x64 ; =>initialized);
            super::concat_cache::emit_record(&mut self.ops, values, regs, destination);
            dynasm!(self.ops ; .arch x64 ; jmp =>done);
        }
        dynasm!(self.ops ; .arch x64 ; =>slow);
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
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }
    /// A BigInt binary operator. Operands that fit `i64` take the inline
    /// BigInt64 path, whose result is carved in the buffer; every other
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
