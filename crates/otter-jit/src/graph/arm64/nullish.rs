//! Non-converting loose nullish equality for AArch64 graph values.
//!
//! # Contents
//! - Immediate and ordinary-cell comparisons produce tagged booleans inline.
//! - Proven ordinary/nullish bounds compare the immediate tags alone.
//! - Native functions use the existing total typeof leaf for HTML-DDA identity.
//!
//! # Invariants
//! - The input remains intact until the result is written, including aliasing.
//! - The cold leaf allocates nothing, executes no JavaScript and preserves all
//!   live registers. The comparison does not invalidate heap facts.
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
        let input = Self::gp(self.allocation.node(node).inputs[0]);
        let destination = Self::gp(self.allocation.node(node).result.expect("a boolean result"));
        if object_or_nullish {
            dynasm!(self.ops ; .arch aarch64
                ; orr x16, X(input), tag::UNDEFINED_TAG
                ; cmp x16, VALUE_UNDEFINED as u32
            );
            if let Some(sink) = self.sink {
                let (equal, differ) = if negate {
                    (sink.if_false, sink.if_true)
                } else {
                    (sink.if_true, sink.if_false)
                };
                let equal = self.cond_target(equal);
                dynasm!(self.ops ; .arch aarch64 ; b.eq =>equal ; b =>differ);
            } else {
                if negate {
                    dynasm!(self.ops ; .arch aarch64 ; cset W(destination), ne);
                } else {
                    dynasm!(self.ops ; .arch aarch64 ; cset W(destination), eq);
                }
                dynasm!(self.ops ; .arch aarch64 ; orr XSP(destination), X(destination), VALUE_FALSE);
            }
            return;
        }
        let equal = self.ops.new_dynamic_label();
        let differ = self.ops.new_dynamic_label();
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let native = u32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG);
        dynasm!(self.ops ; .arch aarch64
            ; orr x16, X(input), tag::UNDEFINED_TAG
            ; cmp x16, VALUE_UNDEFINED as u32
            ; b.eq =>equal
            ; tst X(input), CELL_TEST_MASK
            ; b.ne =>differ
            ; cbz X(input), =>differ
            ; ldrb w16, [X(input)]
            ; cmp w16, native
            ; b.eq =>slow
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
            dynasm!(self.ops ; .arch aarch64
                ; b =>on_differ
                ; =>equal
                ; b =>on_equal
                ; =>done
            );
        } else {
            self.load_immediate(destination, if negate { VALUE_TRUE } else { VALUE_FALSE });
            dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>equal);
            self.load_immediate(destination, if negate { VALUE_FALSE } else { VALUE_TRUE });
            dynasm!(self.ops ; .arch aarch64 ; =>done);
        }
        let test = otter_bytecode::TypeOfTest {
            kind: otter_bytecode::TypeOfKind::Undefined,
            negate,
        }
        .encode();
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'a>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>slow);
                let saved = codegen.emit_save_registers(&live);
                dynasm!(codegen.ops ; .arch aarch64
                    ; mov x16, X(input)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                    ; mov x1, x16
                );
                codegen.load_immediate(2, u64::from(test as u32));
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::TYPEOF_TEST_LEAF.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_TYPEOF_TEST_LEAF),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16 ; mov x16, x0);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops ; .arch aarch64 ; mov X(destination), x16 ; b =>done);
            }));
        self.emit_sink_register(destination);
    }
}
