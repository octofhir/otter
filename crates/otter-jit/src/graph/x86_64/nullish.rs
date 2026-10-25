//! Non-converting loose nullish equality for x86 graph values.
//!
//! # Contents
//! - Immediate and ordinary-cell comparisons produce tagged booleans inline.
//! - Proven ordinary/nullish bounds compare the immediate tags alone.
//! - Native functions use the existing total typeof leaf for HTML-DDA identity.
//!
//! # Invariants
//! - The input remains intact until the result is written, including aliasing.
//! - The cold leaf allocates nothing, executes no JavaScript and preserves all
//!   live registers through the platform C boundary.
//! - The comparison does not invalidate heap facts or publish a safepoint.
//!
//! # See also
//! - [`super::super::ir::Kind::NullishEqual`] owns the unary graph contract.
//! - [`otter_vm::runtime_stubs::TYPEOF_TEST_LEAF`] owns native classification.

use super::*;

impl<'a> Codegen<'a> {
    pub(super) fn emit_nullish_equal(
        &mut self,
        node: NodeId,
        negate: bool,
        object_or_nullish: bool,
    ) {
        let input = Self::gp(self.loc(node).inputs[0]);
        let destination = Self::gp(self.loc(node).result.expect("a boolean result"));
        if object_or_nullish {
            dynasm!(self.ops ; .arch x64
                ; mov r10, Rq(input)
                ; or r10, tag::UNDEFINED_TAG as i32
                ; cmp r10, tag::VALUE_UNDEFINED as i32
            );
            if let Some(sink) = self.sink {
                let (equal, differ) = if negate {
                    (sink.if_false, sink.if_true)
                } else {
                    (sink.if_true, sink.if_false)
                };
                dynasm!(self.ops ; .arch x64 ; je =>equal ; jmp =>differ);
            } else {
                if negate {
                    dynasm!(self.ops ; .arch x64 ; setne Rb(destination));
                } else {
                    dynasm!(self.ops ; .arch x64 ; sete Rb(destination));
                }
                dynasm!(self.ops ; .arch x64
                    ; movzx Rd(destination), Rb(destination)
                    ; or Rq(destination), tag::VALUE_FALSE as i32
                );
            }
            return;
        }
        let equal = self.ops.new_dynamic_label();
        let differ = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; mov r10, Rq(input)
            ; or r10, tag::UNDEFINED_TAG as i32
            ; cmp r10, tag::VALUE_UNDEFINED as i32
            ; je =>equal
        );
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(input), r10
            ; jnz =>differ
            ; test Rq(input), Rq(input)
            ; jz =>differ
            ; cmp BYTE [Rq(input)], otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG as i8
            ; je =>slow
            ; =>differ
        );
        if let Some(sink) = self.sink {
            // Fused into its branch: each inline answer jumps to its
            // successor; only the native-function leaf's answer is tested.
            let (on_equal, on_differ) = if negate {
                (sink.if_false, sink.if_true)
            } else {
                (sink.if_true, sink.if_false)
            };
            dynasm!(self.ops ; .arch x64
                ; jmp =>on_differ
                ; =>equal
                ; jmp =>on_equal
                ; =>done
            );
        } else {
            self.load_immediate(
                destination,
                if negate {
                    tag::VALUE_TRUE
                } else {
                    tag::VALUE_FALSE
                },
            );
            dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>equal);
            self.load_immediate(
                destination,
                if negate {
                    tag::VALUE_FALSE
                } else {
                    tag::VALUE_TRUE
                },
            );
            dynasm!(self.ops ; .arch x64 ; =>done);
        }
        let test = otter_bytecode::TypeOfTest {
            kind: otter_bytecode::TypeOfKind::Undefined,
            negate,
        }
        .encode();
        let live = self.loc(node).live_registers.clone();
        self.deferred.push(Box::new(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>slow);
            let saved = codegen.emit_save_registers(&live);
            dynasm!(codegen.ops ; .arch x64
                ; mov r10, Rq(input)
                ; mov rsi, r10
                ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
            );
            codegen.load_immediate(2, u64::from(test as u32));
            codegen.emit_scalar_vm_leaf(
                abi::STUB_TYPEOF_TEST_LEAF,
                otter_vm::runtime_stubs::TYPEOF_TEST_LEAF.entry_addr() as u64,
            );
            dynasm!(codegen.ops ; .arch x64 ; mov r10, rax);
            codegen.emit_restore_registers(&live, saved);
            dynasm!(codegen.ops ; .arch x64 ; mov Rq(destination), r10 ; jmp =>done);
        }));
        self.emit_sink_register(destination);
    }
}
