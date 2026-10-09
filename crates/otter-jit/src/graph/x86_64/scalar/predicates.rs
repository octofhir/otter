//! x86 tagged predicates with cold, typed NoAlloc content probes.
//!
//! # Contents
//! - Numeric and identity strict equality, with content comparison in the VM.
//! - Loose equality: the pairs that convert nothing inline, every coercing
//!   pair through the committed runtime operation.
//! - Truthiness of immediates, Numbers, strings and objects inline, and total
//!   leaf handling for native functions, BigInts and other immediates.
//! - Shared truthiness branches for Graph control.
//! - `typeof` tests through the VM's total leaf probe.
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
            Kind::LooseEqual { negate } => {
                let a = Self::gp(self.loc(node).inputs[0]);
                let b = Self::gp(self.loc(node).inputs[1]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                let double = self.loc(node).fp_temps[0];
                self.emit_loose_equal(node, a, b, double, dst, negate)?;
            }
            Kind::TestTypeOf { test } => {
                let value = Self::gp(self.loc(node).inputs[0]);
                let dst = Self::gp(self.loc(node).result.unwrap());
                self.emit_test_typeof(node, value, dst, test);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn emit_strict_equal(&mut self, node: NodeId, a: u8, b: u8, double: u8, dst: u8, negate: bool) {
        let non_number = self.ops.new_dynamic_label();
        let outcomes = self.equality_outcomes();
        self.load_immediate(10, tag::NUMBER_TAG);
        dynasm!(self.ops ; .arch x64
            ; test Rq(a), r10 ; jz =>non_number
            ; test Rq(b), r10 ; jz =>outcomes.differ
        );
        self.emit_number_pair(a, b, double, outcomes);
        dynasm!(self.ops ; .arch x64 ; =>non_number);
        self.load_immediate(10, tag::NUMBER_TAG);
        dynasm!(self.ops ; .arch x64
            ; test Rq(b), r10 ; jnz =>outcomes.differ
            ; cmp Rq(a), Rq(b) ; je =>outcomes.equal
        );
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(a), r10 ; jnz =>outcomes.differ
            ; test Rq(b), r10 ; jnz =>outcomes.differ
            ; test Rq(a), Rq(a) ; jz =>outcomes.differ
            ; test Rq(b), Rq(b) ; jz =>outcomes.differ
            ; movzx r10d, BYTE [Rq(a)] ; movzx r11d, BYTE [Rq(b)]
            ; cmp r10d, r11d ; jne =>outcomes.differ
        );
        self.emit_same_type_cells(a, b, outcomes);
        let done = self.ops.new_dynamic_label();
        self.emit_equality_results(node, a, b, dst, negate, outcomes, done);
        dynasm!(self.ops ; .arch x64 ; =>done);
    }

    /// `Rq(dst)` = `Rq(a) == Rq(b)` (`!=` when `negate`) as a tagged
    /// boolean: identity, two Numbers, `null`/`undefined` and two cells that
    /// convert nothing inline; every coercing pair, and `null`/`undefined`
    /// against a native function (the only possible `[[IsHTMLDDA]]`), in the
    /// runtime, which completes the published `==`/`!=` itself.
    fn emit_loose_equal(
        &mut self,
        node: NodeId,
        a: u8,
        b: u8,
        double: u8,
        dst: u8,
        negate: bool,
    ) -> Result<(), Unsupported> {
        const NULLISH_BIT: i32 = 0x8;
        const NULLISH: i32 = tag::VALUE_UNDEFINED as i32;
        const _: () = assert!(tag::VALUE_NULL | NULLISH_BIT as u64 == tag::VALUE_UNDEFINED);
        const _: () = assert!(tag::VALUE_UNDEFINED | NULLISH_BIT as u64 == tag::VALUE_UNDEFINED);
        let a_number = self.ops.new_dynamic_label();
        let b_number = self.ops.new_dynamic_label();
        let both_numbers = self.ops.new_dynamic_label();
        let a_nullish = self.ops.new_dynamic_label();
        let b_nullish = self.ops.new_dynamic_label();
        let nullish_against = self.ops.new_dynamic_label();
        let same_type = self.ops.new_dynamic_label();
        let runtime = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let outcomes = self.equality_outcomes();
        let native_tag = i32::from(otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG);
        self.load_immediate(10, tag::NUMBER_TAG);
        dynasm!(self.ops ; .arch x64
            ; test Rq(a), r10 ; jnz =>a_number
            ; test Rq(b), r10 ; jnz =>b_number
            // Neither is a Number: identical words are equal.
            ; cmp Rq(a), Rq(b) ; je =>outcomes.equal
            ; mov r11, Rq(a) ; or r11, NULLISH_BIT ; cmp r11, NULLISH ; je =>a_nullish
            ; mov r11, Rq(b) ; or r11, NULLISH_BIT ; cmp r11, NULLISH ; je =>b_nullish
        );
        // A boolean or function-id immediate converts or compares against a
        // converted cell: the runtime decides.
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(a), r10 ; jnz =>runtime
            ; test Rq(b), r10 ; jnz =>runtime
            ; test Rq(a), Rq(a) ; jz =>runtime
            ; test Rq(b), Rq(b) ; jz =>runtime
            ; movzx r10d, BYTE [Rq(a)] ; movzx r11d, BYTE [Rq(b)]
            ; cmp r10d, r11d ; je =>same_type
        );
        // Cells of two types: a primitive on either side converts the other;
        // two objects are equal only when identical, and these are not.
        for primitive in self.view.primitive_cell_type_tags {
            dynasm!(self.ops ; .arch x64
                ; cmp r10d, DWORD i32::from(primitive) ; je =>runtime
                ; cmp r11d, DWORD i32::from(primitive) ; je =>runtime
            );
        }
        dynasm!(self.ops ; .arch x64 ; jmp =>outcomes.differ ; =>same_type);
        self.emit_same_type_cells(a, b, outcomes);
        dynasm!(self.ops ; .arch x64
            // `null`/`undefined` equals only the other of the two, or an
            // `[[IsHTMLDDA]]` native function.
            ; =>a_nullish
            ; mov r11, Rq(b) ; or r11, NULLISH_BIT ; cmp r11, NULLISH ; je =>outcomes.equal
            ; mov r11, Rq(b) ; jmp =>nullish_against
            ; =>b_nullish
            ; mov r11, Rq(a)
            ; =>nullish_against
        );
        self.load_immediate(10, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test r11, r10 ; jnz =>outcomes.differ
            ; test r11, r11 ; jz =>outcomes.differ
            ; cmp BYTE [r11], native_tag as i8 ; je =>runtime
            ; jmp =>outcomes.differ
            // A Number against `null`/`undefined` differs; against anything
            // else that is not a Number, it converts.
            ; =>b_number
            ; mov r11, Rq(a) ; or r11, NULLISH_BIT ; cmp r11, NULLISH ; je =>outcomes.differ
            ; jmp =>runtime
            ; =>a_number
            ; test Rq(b), r10 ; jnz =>both_numbers
            ; mov r11, Rq(b) ; or r11, NULLISH_BIT ; cmp r11, NULLISH ; je =>outcomes.differ
            ; jmp =>runtime
            ; =>both_numbers
        );
        self.emit_number_pair(a, b, double, outcomes);
        self.emit_equality_results(node, a, b, dst, negate, outcomes, done);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>runtime);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
            &[CommittedArgument::Value(a), CommittedArgument::Value(b)],
            Some(dst),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    fn equality_outcomes(&mut self) -> EqualityOutcomes {
        EqualityOutcomes {
            equal: self.ops.new_dynamic_label(),
            differ: self.ops.new_dynamic_label(),
            leaf: self.ops.new_dynamic_label(),
        }
    }

    /// Two Numbers in `Rq(a)` and `Rq(b)` by value, to `outcomes.equal` or
    /// `outcomes.differ`; NaN is unordered.
    fn emit_number_pair(&mut self, a: u8, b: u8, double: u8, outcomes: EqualityOutcomes) {
        let a_int = self.ops.new_dynamic_label();
        let a_ready = self.ops.new_dynamic_label();
        let b_int = self.ops.new_dynamic_label();
        let b_ready = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
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
            ; ucomisd xmm15, Rx(double) ; jp =>outcomes.differ ; je =>outcomes.equal
            ; jmp =>outcomes.differ
        );
    }

    /// Two distinct cells whose type tags, in `r10d`, are equal: strings and
    /// BigInts compare by content, every other type by identity.
    fn emit_same_type_cells(&mut self, a: u8, b: u8, outcomes: EqualityOutcomes) {
        dynasm!(self.ops ; .arch x64
            ; cmp r10d, DWORD otter_vm::bigint::BIG_INT_BODY_TYPE_TAG as i32 ; je =>outcomes.leaf
            ; cmp r10d, DWORD otter_vm::string::JS_STRING_BODY_TYPE_TAG as i32
            ; jne =>outcomes.differ
        );
        // Strings of different lengths differ without reading their units,
        // and two strings with recorded atoms are equal exactly when their
        // atoms are.
        let length = self.view.string_layout.string_len_byte as i32;
        let atom = self.view.string_layout.atom_byte as i32;
        dynasm!(self.ops ; .arch x64
            ; mov r10d, DWORD [Rq(a) + length] ; cmp r10d, DWORD [Rq(b) + length]
            ; jne =>outcomes.differ
            ; mov r10d, DWORD [Rq(a) + atom] ; test r10d, r10d ; jz =>outcomes.leaf
            ; mov r11d, DWORD [Rq(b) + atom] ; test r11d, r11d ; jz =>outcomes.leaf
            ; cmp r10d, r11d ; je =>outcomes.equal ; jmp =>outcomes.differ
        );
    }

    /// Bind `outcomes.equal` and `outcomes.differ` to the tagged results,
    /// the first continuing at `done`, the second falling through; defer
    /// the content leaf of `outcomes.leaf`.
    #[allow(clippy::too_many_arguments)]
    fn emit_equality_results(
        &mut self,
        node: NodeId,
        a: u8,
        b: u8,
        dst: u8,
        negate: bool,
        outcomes: EqualityOutcomes,
        done: DynamicLabel,
    ) {
        let (equal_value, differ_value) = if negate {
            (tag::VALUE_FALSE, tag::VALUE_TRUE)
        } else {
            (tag::VALUE_TRUE, tag::VALUE_FALSE)
        };
        dynasm!(self.ops ; .arch x64 ; =>outcomes.equal);
        self.load_immediate(dst, equal_value);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>outcomes.differ);
        self.load_immediate(dst, differ_value);
        self.defer_predicate_leaf(
            node,
            a,
            Some(b),
            outcomes.leaf,
            outcomes.equal,
            outcomes.differ,
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
        let not_int = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            ; cmp Rq(value), DWORD tag::VALUE_TRUE as i32 ; je =>if_true
            ; cmp Rq(value), DWORD tag::VALUE_FALSE as i32 ; je =>if_false
            ; cmp Rq(value), DWORD tag::VALUE_UNDEFINED as i32 ; je =>if_false
            ; cmp Rq(value), DWORD tag::VALUE_NULL as i32 ; je =>if_false
            ; mov r10, Rq(value) ; sar r10, 49 ; cmp r10, -1 ; jne =>not_int
            ; test Rd(value), Rd(value) ; jz =>if_false ; jmp =>if_true
            ; =>not_int
        );
        self.emit_heap_truthiness(value, if_true, if_false, slow);
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

    /// `ToBoolean` of a value outside the int32 tag range that is not a
    /// boolean, `null` or `undefined`: a double is falsy at ±0 and NaN, a
    /// string at length zero, and every other cell is an object, truthy,
    /// except a native function (the only possible `[[IsHTMLDDA]]`) and a
    /// BigInt, which go to `slow` with the remaining immediates. `r10` holds
    /// the value's top bits shifted down by 49. Clobbers `r10` and `r11`.
    fn emit_heap_truthiness(
        &mut self,
        value: u8,
        truthy: DynamicLabel,
        falsy: DynamicLabel,
        slow: DynamicLabel,
    ) {
        let cell = self.ops.new_dynamic_label();
        let object = self.ops.new_dynamic_label();
        let string_tag = self.view.string_layout.string_type_tag as i8;
        let length = self.view.string_layout.string_len_byte as i32;
        self.load_immediate(11, tag::NOT_CELL_MASK);
        dynasm!(self.ops ; .arch x64
            ; test Rq(value), r11 ; jz =>cell
            // A boxed double carries number tag bits; other immediates do not.
            ; test r10, r10 ; jz =>slow
        );
        self.load_immediate(11, tag::DOUBLE_ENCODE_OFFSET);
        dynasm!(self.ops ; .arch x64
            ; mov r10, Rq(value) ; sub r10, r11
            ; mov r11, r10 ; shl r11, 1 ; jz =>falsy
        );
        // NaN boxes as the one canonical pattern.
        self.load_immediate(11, tag::CANONICAL_NAN);
        dynasm!(self.ops ; .arch x64
            ; cmp r10, r11 ; je =>falsy ; jmp =>truthy
            ; =>cell
            ; test Rq(value), Rq(value) ; jz =>slow
            ; cmp BYTE [Rq(value)], string_tag ; jne =>object
            ; cmp DWORD [Rq(value) + length], 0 ; je =>falsy ; jmp =>truthy
            ; =>object
            ; cmp BYTE [Rq(value)], otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG as i8
            ; je =>slow
            ; cmp BYTE [Rq(value)], otter_vm::bigint::BIG_INT_BODY_TYPE_TAG as i8
            ; je =>slow
            ; jmp =>truthy
        );
    }

    #[allow(clippy::too_many_arguments)]
    /// `Rq(dst)` = `typeof Rq(value)` compared with the encoded
    /// `TypeOfTest`, as the leaf probe decides it. The probe reads the cell
    /// and allocates nothing; its only miss is a null heap, which generated
    /// code never passes.
    fn emit_test_typeof(&mut self, node: NodeId, value: u8, dst: u8, test: i32) {
        let live = self.loc(node).live_registers.clone();
        let saved = self.emit_save_registers(&live);
        dynasm!(self.ops ; .arch x64
            ; mov rsi, Rq(value)
            ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
        );
        self.load_immediate(2, u64::from(test as u32));
        self.emit_scalar_vm_leaf(
            abi::STUB_TYPEOF_TEST_LEAF,
            otter_vm::runtime_stubs::TYPEOF_TEST_LEAF.entry_addr() as u64,
        );
        dynasm!(self.ops ; .arch x64 ; mov r10, rax);
        self.emit_restore_registers(&live, saved);
        dynasm!(self.ops ; .arch x64 ; mov Rq(dst), r10);
    }

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

/// Where an equality's inline tests send control.
#[derive(Clone, Copy)]
struct EqualityOutcomes {
    equal: DynamicLabel,
    differ: DynamicLabel,
    leaf: DynamicLabel,
}
