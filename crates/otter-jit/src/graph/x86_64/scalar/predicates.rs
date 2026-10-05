//! x86 tagged predicates with cold, typed NoAlloc content probes.
//!
//! # Contents
//! - Numeric and identity strict equality, with content comparison in the VM.
//! - Immediate/int32 truthiness and total leaf handling for other values.
//! - Shared truthiness branches for Graph control.
//!
//! # Invariants
//! - Inputs remain intact until the final tagged boolean is committed.
//! - Slow leaves see a live heap and preserve every control-flow-live register.
//! - Numeric equality distinguishes NaN from identical words and equates ±0.
//!
//! # See also
//! - `otter_vm::runtime_stubs` owns total value semantics and leaf signatures.

use super::*;

impl<'a> Codegen<'a> {
    pub(super) fn emit_predicate(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        match self.graph.node(node).kind.clone() {
            Kind::ToBoolean | Kind::LogicalNot => {
                let value = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let negate = self.graph.node(node).kind == Kind::LogicalNot;
                if self
                    .graph
                    .node(self.graph.node(node).inputs[0])
                    .kind
                    .produces_boolean()
                {
                    dynasm!(self.ops ; .arch x64 ; mov Rq(dst), Rq(value));
                    if negate {
                        dynasm!(self.ops ; .arch x64 ; xor Rq(dst), 1);
                    }
                } else {
                    self.emit_to_boolean(node, value, dst, negate);
                }
            }
            Kind::StrictEqual { negate } => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let b = Self::gp(self.loc(node).inputs[1]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let double = self.loc(node).fp_temps[0];
                self.emit_strict_equal(node, a, b, double, dst, negate);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn emit_strict_equal(&mut self, node: NodeId, a: u8, b: u8, double: u8, dst: u8, negate: bool) {
        let non_number = self.ops.new_dynamic_label();
        let a_int = self.ops.new_dynamic_label();
        let a_ready = self.ops.new_dynamic_label();
        let b_int = self.ops.new_dynamic_label();
        let b_ready = self.ops.new_dynamic_label();
        let equal = self.ops.new_dynamic_label();
        let differ = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.load_immediate(10, tag::NUMBER_TAG);
        dynasm!(self.ops ; .arch x64
            ; test Rq(a), r10 ; jz =>non_number
            ; test Rq(b), r10 ; jz =>differ
            ; mov r11, Rq(a) ; sar r11, 49 ; cmp r11, -1 ; je =>a_int
        );
        self.load_immediate(11, tag::DOUBLE_ENCODE_OFFSET);
        dynasm!(self.ops ; .arch x64
            ; mov r10, Rq(a) ; sub r10, r11 ; movq xmm15, r10 ; jmp =>a_ready
            ; =>a_int ; cvtsi2sd xmm15, Rd(a) ; =>a_ready
            ; mov r11, Rq(b) ; sar r11, 49 ; cmp r11, -1 ; je =>b_int
        );
        self.load_immediate(11, tag::DOUBLE_ENCODE_OFFSET);
        dynasm!(self.ops ; .arch x64
            ; mov r10, Rq(b) ; sub r10, r11 ; movq Rx(double), r10 ; jmp =>b_ready
            ; =>b_int ; cvtsi2sd Rx(double), Rd(b) ; =>b_ready
            ; ucomisd xmm15, Rx(double) ; jp =>differ ; je =>equal ; jmp =>differ
            ; =>non_number
        );
        self.load_immediate(10, tag::NUMBER_TAG);
        dynasm!(self.ops ; .arch x64
            ; test Rq(b), r10 ; jnz =>differ
            ; cmp Rq(a), Rq(b) ; je =>equal
        );
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(a), r10 ; jnz =>differ
            ; test Rq(b), r10 ; jnz =>differ
            ; test Rq(a), Rq(a) ; jz =>differ
            ; test Rq(b), Rq(b) ; jz =>differ
            ; movzx r10d, BYTE [Rq(a)] ; movzx r11d, BYTE [Rq(b)]
            ; cmp r10d, r11d ; jne =>differ
            ; cmp r10d, DWORD otter_vm::string::JS_STRING_BODY_TYPE_TAG as i32 ; je =>slow
            ; cmp r10d, DWORD otter_vm::bigint::BIG_INT_BODY_TYPE_TAG as i32 ; je =>slow
            ; jmp =>differ
        );
        let (equal_value, differ_value) = if negate {
            (tag::VALUE_FALSE, tag::VALUE_TRUE)
        } else {
            (tag::VALUE_TRUE, tag::VALUE_FALSE)
        };
        dynasm!(self.ops ; .arch x64 ; =>equal);
        self.load_immediate(dst, equal_value);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>differ);
        self.load_immediate(dst, differ_value);
        dynasm!(self.ops ; .arch x64 ; =>done);
        self.defer_predicate_leaf(
            node,
            a,
            Some(b),
            slow,
            equal,
            differ,
            abi::STUB_STRICT_EQ_LEAF,
            otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
        );
    }

    pub(in super::super) fn emit_to_boolean(
        &mut self,
        node: NodeId,
        value: u8,
        dst: u8,
        negate: bool,
    ) {
        let truthy = self.ops.new_dynamic_label();
        let falsy = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.emit_truthy_branch(node, value, truthy, falsy);
        dynasm!(self.ops ; .arch x64 ; =>truthy);
        self.load_immediate(
            dst,
            if negate {
                tag::VALUE_FALSE
            } else {
                tag::VALUE_TRUE
            },
        );
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>falsy);
        self.load_immediate(
            dst,
            if negate {
                tag::VALUE_TRUE
            } else {
                tag::VALUE_FALSE
            },
        );
        dynasm!(self.ops ; .arch x64 ; =>done);
    }

    pub(in super::super) fn emit_truthy_branch(
        &mut self,
        node: NodeId,
        value: u8,
        if_true: DynamicLabel,
        if_false: DynamicLabel,
    ) {
        let slow = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; cmp Rq(value), DWORD tag::VALUE_TRUE as i32 ; je =>if_true
            ; cmp Rq(value), DWORD tag::VALUE_FALSE as i32 ; je =>if_false
            ; cmp Rq(value), DWORD tag::VALUE_UNDEFINED as i32 ; je =>if_false
            ; cmp Rq(value), DWORD tag::VALUE_NULL as i32 ; je =>if_false
            ; mov r10, Rq(value) ; sar r10, 49 ; cmp r10, -1 ; jne =>slow
            ; test Rd(value), Rd(value) ; jz =>if_false ; jmp =>if_true
        );
        self.defer_predicate_leaf(
            node,
            value,
            None,
            slow,
            if_true,
            if_false,
            abi::STUB_TO_BOOLEAN_LEAF,
            otter_vm::runtime_stubs::TO_BOOLEAN_LEAF.entry_addr() as u64,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn defer_predicate_leaf(
        &mut self,
        node: NodeId,
        a: u8,
        b: Option<u8>,
        slow: DynamicLabel,
        if_true: DynamicLabel,
        if_false: DynamicLabel,
        stub: abi::RuntimeStubDescriptor,
        entry: u64,
    ) {
        let live = self.loc(node).live_registers.clone();
        self.deferred.push(Box::new(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>slow);
            let saved = codegen.emit_save_registers(&live);
            // Park both sources before preparing potentially aliased ABI words.
            dynasm!(codegen.ops ; .arch x64 ; mov r10, Rq(a));
            if let Some(b) = b {
                dynasm!(codegen.ops ; .arch x64 ; mov r11, Rq(b));
            } else {
                codegen.load_immediate(11, tag::VALUE_UNDEFINED);
            }
            dynasm!(codegen.ops ; .arch x64
                ; mov rsi, r10 ; mov rdx, r11
                ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
            );
            codegen.emit_scalar_vm_leaf(stub, entry);
            dynasm!(codegen.ops ; .arch x64 ; mov r10, rax);
            codegen.emit_restore_registers(&live, saved);
            dynasm!(codegen.ops ; .arch x64
                ; cmp r10, DWORD tag::VALUE_TRUE as i32 ; je =>if_true ; jmp =>if_false
            );
        }));
    }
}
