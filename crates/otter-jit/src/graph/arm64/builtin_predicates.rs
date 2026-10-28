//! Builtin predicates a registry reduction lowers to on AArch64:
//! `Number.is*` and `Array.isArray` of a tagged value.
//!
//! # Contents
//! - [`Codegen::emit_number_test`] — the four `Number` predicates inline: an
//!   int32 answers by the test alone, a double by its float64 value.
//! - [`Codegen::emit_is_array`] — an Array body's type tag; a Proxy leaves
//!   through the node's eager deopt.
//!
//! # Invariants
//! - The input is read before the result is written, which may share its
//!   register. Only `x16`/`x17`, `d31` and the node's float temp are clobbered.
//! - Neither predicate allocates, calls or converts its input.
//!
//! # See also
//! - [`otter_vm::jit_static_native::JitBuiltinReduction`] — the row that
//!   selects these nodes.

use super::*;
use otter_vm::jit_static_native::JitNumberTest;

impl Codegen<'_> {
    /// `X(destination)` = the tagged boolean of `test` on `X(value)`, with
    /// `D(double)` as scratch.
    pub(super) fn emit_number_test(
        &mut self,
        value: u8,
        destination: u8,
        double: u8,
        test: JitNumberTest,
    ) {
        let int = self.ops.new_dynamic_label();
        let yes = self.ops.new_dynamic_label();
        let no = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.load_immediate(16, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(value), x16
            ; b.eq =>no
            ; cmp X(value), x16
            ; b.hs =>int
        );
        self.load_immediate(17, DOUBLE_OFFSET);
        dynasm!(self.ops
            ; .arch aarch64
            ; sub x17, X(value), x17
            ; fmov D(double), x17
        );
        match test {
            JitNumberTest::NaN => dynasm!(self.ops
                ; .arch aarch64
                ; fcmp D(double), D(double)
                ; b.vs =>yes
                ; b =>no
            ),
            // Infinity minus itself is NaN, as is NaN minus anything.
            JitNumberTest::Finite => dynasm!(self.ops
                ; .arch aarch64
                ; fsub d31, D(double), D(double)
                ; fcmp d31, d31
                ; b.vs =>no
                ; b =>yes
            ),
            JitNumberTest::Integer | JitNumberTest::SafeInteger => {
                dynasm!(self.ops
                    ; .arch aarch64
                    ; fsub d31, D(double), D(double)
                    ; fcmp d31, d31
                    ; b.vs =>no
                    ; frintz d31, D(double)
                    ; fcmp d31, D(double)
                    ; b.ne =>no
                );
                if test == JitNumberTest::SafeInteger {
                    self.load_immediate(17, 9_007_199_254_740_991_f64.to_bits());
                    dynasm!(self.ops
                        ; .arch aarch64
                        ; fabs D(double), D(double)
                        ; fmov d31, x17
                        ; fcmp D(double), d31
                        ; b.hi =>no
                    );
                }
                dynasm!(self.ops ; .arch aarch64 ; b =>yes);
            }
        }
        dynasm!(self.ops ; .arch aarch64 ; =>int);
        if test == JitNumberTest::NaN {
            dynasm!(self.ops ; .arch aarch64 ; b =>no);
        }
        dynasm!(self.ops ; .arch aarch64 ; =>yes);
        self.load_immediate(destination, VALUE_TRUE);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>no);
        self.load_immediate(destination, VALUE_FALSE);
        dynasm!(self.ops ; .arch aarch64 ; =>done);
    }

    /// `X(destination)` = whether `X(value)` is an Array body, as a tagged
    /// boolean. A Proxy, whose answer is its target's, takes `exit`.
    pub(super) fn emit_is_array(&mut self, value: u8, destination: u8, exit: DynamicLabel) {
        let yes = self.ops.new_dynamic_label();
        let no = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(value), CELL_TEST_MASK
            ; b.ne =>no
            ; cbz X(value), =>no
            ; ldrb w16, [X(value)]
            ; cmp w16, u32::from(otter_vm::array::ARRAY_BODY_TYPE_TAG)
            ; b.eq =>yes
            ; cmp w16, u32::from(otter_vm::proxy::PROXY_BODY_TYPE_TAG)
            ; b.eq =>exit
            ; =>no
        );
        self.load_immediate(destination, VALUE_FALSE);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>yes);
        self.load_immediate(destination, VALUE_TRUE);
        dynasm!(self.ops ; .arch aarch64 ; =>done);
    }
}
