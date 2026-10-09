//! Equality operators of the graph tier: `===` and `==` over any two tagged
//! values, producing a tagged boolean.
//!
//! # Contents
//! - [`Codegen::emit_strict_equal`] — §7.2.16 IsStrictlyEqual inline, with
//!   the string/BigInt content leaf out of line.
//! - [`Codegen::emit_loose_equal`] — §7.2.15 IsLooselyEqual: every case
//!   that coerces nothing inline, every coercing case (and `[[IsHTMLDDA]]`)
//!   in the runtime, as V8's `Equal` builtin splits them.
//!
//! # Invariants
//! - Both operands are read before the result is written; the result may
//!   share a register with an operand.
//! - Numbers compare by value whatever their encoding; NaN is unordered.
//! - Two strings with recorded atoms are equal exactly when their atoms are;
//!   any other pair of strings or BigInts of one type compares by content in
//!   the no-allocation leaf, which keeps every live register.
//! - Loose equality reaches the runtime only for an operand pair whose
//!   comparison may run `ToPrimitive`/`ToNumber`, or for `null`/`undefined`
//!   against a native function (the only cell that can be `[[IsHTMLDDA]]`).
//!   The runtime completes the published `==`/`!=` itself, negation
//!   included.
//!
//! # See also
//! - `otter_vm::Interpreter::loose_equal_with_context` — the runtime case.
//! - [`super::super::ir::Kind::LooseEqual`] — the node.

use super::{
    Codegen, CommittedArgument, Condition, DOUBLE_OFFSET, DynamicLabel, NOT_CELL_MASK, NUMBER_TAG,
    NodeId, RelocationTarget, THREAD_OFFSET, VALUE_FALSE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED,
    VM_THREAD_GC_HEAP_OFFSET, abi, emit_load_symbol_u64,
};
use crate::Unsupported;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};

/// `null | NULLISH_BIT == undefined | NULLISH_BIT == NULLISH`; no other
/// value maps there (cells never carry the immediate tag bit).
const NULLISH_BIT: u64 = 0x8;
const NULLISH: u32 = VALUE_UNDEFINED as u32;
const _: () = assert!(VALUE_NULL | NULLISH_BIT == VALUE_UNDEFINED);
const _: () = assert!(VALUE_UNDEFINED | NULLISH_BIT == VALUE_UNDEFINED);
const _: () = assert!(VALUE_TRUE | NULLISH_BIT != VALUE_UNDEFINED);
const _: () = assert!(VALUE_FALSE | NULLISH_BIT != VALUE_UNDEFINED);

/// Where an equality's inline tests send control.
#[derive(Clone, Copy)]
struct Outcomes {
    equal: DynamicLabel,
    differ: DynamicLabel,
    leaf: DynamicLabel,
}

impl Codegen<'_> {
    /// `X(destination)` = the tagged boolean of `X(a) === X(b)` under
    /// `condition` (`Equal` for `===`, `NotEqual` for `!==`). Numbers compare
    /// by value, identical words are equal, two cells of one string or BigInt
    /// type compare by content through the leaf probe, and anything else
    /// differs.
    pub(super) fn emit_strict_equal(
        &mut self,
        node: NodeId,
        [a, b]: [u8; 2],
        double: u8,
        destination: u8,
        condition: Condition,
    ) {
        let lhs_non_number = self.ops.new_dynamic_label();
        let cells = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let outcomes = self.equality_outcomes();
        self.load_immediate(16, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(a), x16
            ; b.eq =>lhs_non_number
            ; tst X(b), x16
            ; b.eq =>outcomes.differ
        );
        self.emit_number_pair_equal([a, b], double, destination, condition);
        dynasm!(self.ops
            ; .arch aarch64
            ; b =>done
            ; =>lhs_non_number
            ; tst X(b), x16
            ; b.ne =>outcomes.differ
        );
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(a), x16
            ; b.eq =>cells
            ; tst X(b), x16
            ; b.eq =>cells
            // Two immediates: identity.
            ; cmp X(a), X(b)
            ; b.eq =>outcomes.equal
            ; b =>outcomes.differ
            ; =>cells
            ; cmp X(a), X(b)
            ; b.eq =>outcomes.equal
            // A cell differs from an immediate, and two cells of different
            // types, or of a type compared by identity, differ.
            ; tst X(a), x16
            ; b.ne =>outcomes.differ
            ; tst X(b), x16
            ; b.ne =>outcomes.differ
            ; ldrb w16, [X(a)]
            ; ldrb w17, [X(b)]
            ; cmp w16, w17
            ; b.ne =>outcomes.differ
        );
        self.emit_same_type_cells([a, b], outcomes);
        self.emit_equality_results(node, [a, b], destination, condition, outcomes, done);
        dynasm!(self.ops ; .arch aarch64 ; =>done);
    }

    /// `X(destination)` = the tagged boolean of `X(a) == X(b)`, negated
    /// when `negate` (`!=`). Identity, two Numbers, `null`/`undefined`
    /// against anything, and two cells that need no conversion decide
    /// inline; every coercing pair completes in the runtime.
    pub(super) fn emit_loose_equal(
        &mut self,
        node: NodeId,
        [a, b]: [u8; 2],
        double: u8,
        destination: u8,
        negate: bool,
    ) -> Result<(), Unsupported> {
        let condition = if negate {
            Condition::NotEqual
        } else {
            Condition::Equal
        };
        let a_number = self.ops.new_dynamic_label();
        let b_number = self.ops.new_dynamic_label();
        let a_nullish = self.ops.new_dynamic_label();
        let b_nullish = self.ops.new_dynamic_label();
        let same_type = self.ops.new_dynamic_label();
        let nullish_against = self.ops.new_dynamic_label();
        let both_numbers = self.ops.new_dynamic_label();
        let runtime = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let outcomes = self.equality_outcomes();
        let native_tag = u32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG);
        self.load_immediate(16, NUMBER_TAG);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(a), x16
            ; b.ne =>a_number
            ; tst X(b), x16
            ; b.ne =>b_number
            // Neither is a Number: identical words are equal.
            ; cmp X(a), X(b)
            ; b.eq =>outcomes.equal
            ; orr x17, X(a), NULLISH_BIT
            ; cmp x17, NULLISH
            ; b.eq =>a_nullish
            ; orr x17, X(b), NULLISH_BIT
            ; cmp x17, NULLISH
            ; b.eq =>b_nullish
        );
        // A boolean or function-id immediate converts or compares against
        // a converted cell: the runtime decides.
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst X(a), x16
            ; b.ne =>runtime
            ; tst X(b), x16
            ; b.ne =>runtime
            ; ldrb w16, [X(a)]
            ; ldrb w17, [X(b)]
            ; cmp w16, w17
            ; b.eq =>same_type
        );
        // Cells of two types: a primitive on either side converts the other;
        // two objects are equal only when identical, and these are not.
        for tag in self.view.primitive_cell_type_tags {
            dynasm!(self.ops
                ; .arch aarch64
                ; cmp w16, u32::from(tag)
                ; b.eq =>runtime
                ; cmp w17, u32::from(tag)
                ; b.eq =>runtime
            );
        }
        dynasm!(self.ops ; .arch aarch64 ; b =>outcomes.differ ; =>same_type);
        self.emit_same_type_cells([a, b], outcomes);
        dynasm!(self.ops
            ; .arch aarch64
            // `null`/`undefined` equals only the other of the two, or an
            // `[[IsHTMLDDA]]` native function.
            ; =>a_nullish
            ; orr x17, X(b), NULLISH_BIT
            ; cmp x17, NULLISH
            ; b.eq =>outcomes.equal
            ; mov x17, X(b)
            ; b =>nullish_against
            ; =>b_nullish
            ; mov x17, X(a)
            ; =>nullish_against
        );
        self.load_immediate(16, NOT_CELL_MASK);
        dynasm!(self.ops
            ; .arch aarch64
            ; tst x17, x16
            ; b.ne =>outcomes.differ
            ; ldrb w16, [x17]
            ; cmp w16, native_tag
            ; b.eq =>runtime
            ; b =>outcomes.differ
            // A Number against `null`/`undefined` differs; against anything
            // else that is not a Number, it converts.
            ; =>b_number
            ; orr x17, X(a), NULLISH_BIT
            ; cmp x17, NULLISH
            ; b.eq =>outcomes.differ
            ; b =>runtime
            ; =>a_number
            ; tst X(b), x16
            ; b.ne =>both_numbers
            ; orr x17, X(b), NULLISH_BIT
            ; cmp x17, NULLISH
            ; b.eq =>outcomes.differ
            ; b =>runtime
            ; =>both_numbers
        );
        self.emit_number_pair_equal([a, b], double, destination, condition);
        dynasm!(self.ops ; .arch aarch64 ; b =>done);
        self.emit_equality_results(node, [a, b], destination, condition, outcomes, done);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>runtime);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
            &[CommittedArgument::Value(a), CommittedArgument::Value(b)],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch aarch64 ; =>done);
        Ok(())
    }

    fn equality_outcomes(&mut self) -> Outcomes {
        Outcomes {
            equal: self.ops.new_dynamic_label(),
            differ: self.ops.new_dynamic_label(),
            leaf: self.ops.new_dynamic_label(),
        }
    }

    /// The comparison of two Numbers in `X(a)` and `X(b)` by value into
    /// `X(destination)`. `x16` holds `NUMBER_TAG`.
    fn emit_number_pair_equal(
        &mut self,
        [a, b]: [u8; 2],
        double: u8,
        destination: u8,
        condition: Condition,
    ) {
        let a_int = self.ops.new_dynamic_label();
        let a_done = self.ops.new_dynamic_label();
        let b_int = self.ops.new_dynamic_label();
        let b_done = self.ops.new_dynamic_label();
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp X(a), x16
            ; b.hs =>a_int
        );
        self.load_immediate(17, DOUBLE_OFFSET);
        dynasm!(self.ops
            ; .arch aarch64
            ; sub x17, X(a), x17
            ; fmov d31, x17
            ; b =>a_done
            ; =>a_int
            ; scvtf d31, W(a)
            ; =>a_done
            ; cmp X(b), x16
            ; b.hs =>b_int
        );
        self.load_immediate(17, DOUBLE_OFFSET);
        dynasm!(self.ops
            ; .arch aarch64
            ; sub x17, X(b), x17
            ; fmov D(double), x17
            ; b =>b_done
            ; =>b_int
            ; scvtf D(double), W(b)
            ; =>b_done
            ; fcmp d31, D(double)
        );
        self.emit_cset_bool(destination, condition, true);
    }

    /// Two distinct cells whose type tags, in `w16`, are equal: strings and
    /// BigInts compare by content, every other type by identity.
    fn emit_same_type_cells(&mut self, [a, b]: [u8; 2], outcomes: Outcomes) {
        let length = self.view.string_layout.string_len_byte;
        let atom = self.view.string_layout.atom_byte;
        dynasm!(self.ops
            ; .arch aarch64
            ; cmp w16, u32::from(otter_vm::bigint::BIG_INT_BODY_TYPE_TAG)
            ; b.eq =>outcomes.leaf
            ; cmp w16, u32::from(otter_vm::string::JS_STRING_BODY_TYPE_TAG)
            ; b.ne =>outcomes.differ
            // Strings of different lengths differ without reading their
            // units, and two strings with recorded atoms are equal exactly
            // when their atoms are.
            ; ldr w16, [X(a), length]
            ; ldr w17, [X(b), length]
            ; cmp w16, w17
            ; b.ne =>outcomes.differ
            ; ldr w16, [X(a), atom]
            ; ldr w17, [X(b), atom]
            ; cbz w16, =>outcomes.leaf
            ; cbz w17, =>outcomes.leaf
            ; cmp w16, w17
            ; b.eq =>outcomes.equal
            ; b =>outcomes.differ
        );
    }

    /// Bind `outcomes.differ` and `outcomes.equal` to the tagged results
    /// under `condition`: the first continues at `done`, the second falls
    /// through. Defer the content leaf of `outcomes.leaf`.
    fn emit_equality_results(
        &mut self,
        node: NodeId,
        [a, b]: [u8; 2],
        destination: u8,
        condition: Condition,
        outcomes: Outcomes,
        done: DynamicLabel,
    ) {
        let (when_equal, when_differ) = if condition == Condition::Equal {
            (VALUE_TRUE, VALUE_FALSE)
        } else {
            (VALUE_FALSE, VALUE_TRUE)
        };
        dynasm!(self.ops ; .arch aarch64 ; =>outcomes.differ);
        self.load_immediate(destination, when_differ);
        dynasm!(self.ops ; .arch aarch64 ; b =>done ; =>outcomes.equal);
        self.load_immediate(destination, when_equal);
        let live = self.allocation.node(node).live_registers.clone();
        self.deferred
            .push(Box::new(move |codegen: &mut Codegen<'_>| {
                dynasm!(codegen.ops ; .arch aarch64 ; =>outcomes.leaf);
                let saved = codegen.emit_save_registers(&live);
                // The probe reads two string or BigInt bodies and allocates
                // nothing; its only miss is a null heap, which generated
                // code never passes.
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; mov x16, X(a)
                    ; mov x17, X(b)
                    ; ldr x0, [x20, THREAD_OFFSET]
                    ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
                    ; mov x1, x16
                    ; mov x2, x17
                );
                emit_load_symbol_u64(
                    &mut codegen.ops,
                    &mut codegen.relocations,
                    16,
                    otter_vm::runtime_stubs::STRICT_EQ_LEAF.entry_addr() as u64,
                    RelocationTarget::runtime_stub(abi::STUB_STRICT_EQ_LEAF),
                );
                dynasm!(codegen.ops ; .arch aarch64 ; blr x16 ; mov x16, x0);
                codegen.emit_restore_registers(&live, saved);
                dynasm!(codegen.ops
                    ; .arch aarch64
                    ; cmp x16, VALUE_TRUE as u32
                    ; b.eq =>outcomes.equal
                    ; b =>outcomes.differ
                );
            }));
    }
}
