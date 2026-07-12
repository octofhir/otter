//! Sparkplug-style baseline emitter (arm64).
//!
//! Lowers a [`otter_vm::JitCompileSnapshot`] to native arm64 with **no IR, no
//! register allocation, and no deopt** — one linear pass, one emit routine per
//! supported opcode, branch fixups via dynasm dynamic labels. Operands and
//! results flow through the executing frame's register window; compiled code
//! reaches the VM through named runtime stubs on [`otter_vm::Interpreter`] for
//! calls, allocation helpers, property fallbacks, and cooperative backedge
//! polling.
//!
//! # ABI
//! Compiled functions are `extern "C" fn(*mut JitCtx) -> JitRet`. The entry
//! loads the register base from `ctx.regs` into a callee-saved register and
//! addresses all locals off it. A normal `Return` yields `JitRet{value, status:
//! 0}`; a failed typed guard yields `status: 1` (the VM re-runs on the
//! interpreter); a re-entered VM call that threw yields `status: 2` with the
//! error parked in `ctx.error`.
//!
//! # GC contract
//! Framed entries use VM-frame registers; frameless JIT-to-JIT entries reserve
//! windows in the interpreter's fixed flat register stack. Both stores are
//! traced as precise roots for their full live extent. No movable JS pointer is
//! kept only in a machine register across a safepoint; allocating callees remain
//! framed and carry exact safepoint records.
//!
//! # Invariants
//! - **Whole-function opt-in.** Any opcode/operand shape outside the supported
//!   subset aborts the compile with [`Unsupported`]; the VM runs the
//!   interpreter. Compiled code never executes a partial function.
//! - **Guard failure = bail, not deopt.** Non-int32 operands / int32 overflow /
//!   non-boolean branch conditions set `status: 1` and return. Bailing re-runs
//!   the whole function on the interpreter.
//!
//! # See also
//! - `JIT_DESIGN.md` §3.2 (backend), §3.5 (GC contract), §4 Phase 1.

// dynasm 5 normalizes dynamic AArch64 register operands through `Into<u8>`;
// when our register ids are already `u8`, that macro-generated conversion is
// intentionally redundant and outside the source-level emitter's control.
#![allow(clippy::useless_conversion)]

use otter_bytecode::Op;
use otter_vm::{
    Interpreter, JitCompileSnapshot,
    runtime_stubs::{alloc_value_stub_trampoline_pair, leaf_no_alloc_stub2_trampoline_pair},
};

mod abi;
mod artifacts;
mod code;
mod lowering;
mod runtime_ops;
mod value_abi;
use abi::*;
use artifacts::*;
pub use code::BaselineCode;
pub use lowering::Unsupported;
use lowering::*;
use runtime_ops::*;
pub(crate) use value_abi::*;

/// GC header type tag for an ordinary `ObjectBody` (mirrors
/// `otter_vm::object::OBJECT_BODY_TYPE_TAG`). A heap cell is disambiguated by
/// this tag before an inline shape-slot read, since every cell value word is a
/// bare cage offset with no class tag of its own.
pub(crate) const OBJECT_BODY_TYPE_TAG: u32 = 0x11;
/// GC header type tag for a `JsClosureBody` (mirrors
/// `otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG`). Guarded before reading a
/// resolved method's `function_id` so a native callable cell is never misread
/// as a bytecode closure.
pub(crate) const JS_CLOSURE_BODY_TYPE_TAG: u32 = 0x23;

fn refresh_jit_collection_method_ics(ctx: &mut JitCtx, vm: &Interpreter) {
    ctx.collection_method_ics = vm.jit_collection_method_ics_ptr();
    ctx.collection_method_ic_count = vm.jit_collection_method_ics_len();
    // The direct-method inline table can reallocate too; refresh its base with the
    // collection ICs at every reentry so a bridge that grew it leaves the compiled
    // caller a valid pointer.
    ctx.direct_method_inline = vm.jit_direct_method_inline_ptr();
}

#[cfg(target_arch = "aarch64")]
pub(crate) mod arm64;

/// Compile a function view to baseline arm64 code, or report why not.
#[cfg(target_arch = "aarch64")]
pub fn compile(view: &JitCompileSnapshot) -> Result<BaselineCode, Unsupported> {
    arm64::compile(view)
}

/// Non-arm64 stub: the emitter is arm64-only for now.
#[cfg(not(target_arch = "aarch64"))]
pub fn compile(view: &JitCompileSnapshot) -> Result<BaselineCode, Unsupported> {
    let _ = view;
    Err(Unsupported::OperandShape("baseline emitter is arm64-only"))
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    //! Execution tests for the call-free integer subset. They drive compiled
    //! code through a `JitCtx` whose `vm`/`stack`/`context` are null — valid
    //! because these functions never reach a `Call`/`MakeFunction` stub — and a
    //! `regs` pointer at a local register array. Fixed 4-byte instruction stride
    //! keeps branch byte-deltas trivial (`rel = (target - next) * 4`).

    use super::{
        BaselinePlan, JitCtx, JitEntry, JitRet, STATUS_RETURNED, Unsupported, VALUE_FALSE,
        VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, compile, value_tag,
    };
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, JitFunctionCode, jit::JitTestInstruction};

    const STRIDE: u32 = 4;

    enum Exit {
        Returned(u64),
        Bailed,
    }

    fn box_i32(v: i32) -> u64 {
        value_tag::NUMBER_TAG | u64::from(v as u32)
    }
    fn unbox_i32(bits: u64) -> i32 {
        bits as u32 as i32
    }

    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitCompileSnapshot {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| {
                JitTestInstruction::new(*op, idx as u32, idx as u32 * STRIDE, operands.clone())
            })
            .collect();
        let mut view = JitCompileSnapshot::without_feedback(0, 1, 8, instructions);
        view.object_shape_byte = 8;
        view.object_values_ptr_byte = 16;
        view.object_inline_values_byte = 80;
        view.object_slab_len_byte = 88;
        view.object_inline_slot_cap = 2;
        view.jit_proto_byte = 12;
        view.heap_number_type_tag = 0x30;
        view.heap_number_bits_byte = 8;
        view.closure_fid_byte = 8;
        view.closure_upvalues_ptr_byte = 16;
        view
    }

    /// The inline typed-array element path locates the backing buffer's data
    /// pointer and live length inside a `Vec<u8>` via `vec_layout_offsets`
    /// (std does not guarantee the field order). Verify the probe lands on the
    /// real pointer and length words for an independent Vec.
    #[test]
    fn vec_layout_probe_finds_ptr_and_len() {
        let (ptr_off, len_off) = super::arm64::vec_layout_offsets();
        assert_ne!(ptr_off, len_off, "ptr and len must be distinct words");
        assert!(
            ptr_off < 24 && len_off < 24,
            "offsets within the 3-word Vec"
        );
        let mut v: Vec<u8> = Vec::with_capacity(16);
        v.extend_from_slice(&[1, 2, 3, 4, 5]);
        // SAFETY: read one machine word at each probed offset and compare to the
        // public pointer/length; never dereferenced beyond the read itself.
        let base = std::ptr::addr_of!(v).cast::<u8>();
        let read_word = |off: u32| unsafe { base.add(off as usize).cast::<usize>().read() };
        assert_eq!(read_word(ptr_off), v.as_ptr() as usize, "probe ptr word");
        assert_eq!(read_word(len_off), v.len(), "probe len word");
    }

    #[test]
    fn frameless_entry_gate_accepts_only_window_owned_bodies() {
        let window_only = view(&[
            (Op::LoadThis, vec![Operand::Register(0)]),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(
            compile(&window_only)
                .expect("window-only body compiles")
                .frameless_entry_safe()
        );

        let frame_reentry = view(&[
            (
                Op::LooseEqual,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        assert!(
            !compile(&frame_reentry)
                .expect("runtime-operation body compiles")
                .frameless_entry_safe()
        );
    }

    #[test]
    fn loose_equality_inlines_numeric_and_nullish_cases() {
        let nullish = view(&[
            (
                Op::LooseEqual,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [VALUE_NULL, VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0];
        match run(&nullish, &mut regs) {
            Exit::Returned(bits) => assert_eq!(bits, VALUE_TRUE),
            Exit::Bailed => panic!("nullish loose equality bailed"),
        }

        let numeric = view(&[
            (
                Op::LooseNotEqual,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(1), box_i32(2), 0, 0, 0, 0, 0, 0];
        match run(&numeric, &mut regs) {
            Exit::Returned(bits) => assert_eq!(bits, VALUE_TRUE),
            Exit::Bailed => panic!("numeric loose inequality bailed"),
        }
    }

    // CodeBlock branch encoding: target instruction = current + 1 + rel.
    fn rel(from: usize, to: usize) -> i32 {
        to as i32 - from as i32 - 1
    }

    fn run(view: &JitCompileSnapshot, regs: &mut [u64]) -> Exit {
        let code = compile(view).expect("compiles");
        let mut error = None;
        let array_index_accessor_protector = false;
        // Probe storage for the inline back-edge poll: an unset interrupt byte
        // and a fuel counter high enough that these small test loops never reach
        // the (null-`vm`) re-entry stub.
        let interrupt_probe: u8 = 0;
        let mut backedge_fuel_probe: u64 = 1 << 30;
        let mut ctx = JitCtx {
            regs: regs.as_mut_ptr(),
            self_closure: 0,
            this_value: 0,
            thread: std::ptr::null_mut(),
            native_frame: std::ptr::null_mut(),
            frame_index: 0,
            upvalues_ptr: 0,
            resume_pc: 0,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
            reg_stack_base: std::ptr::null_mut(),
            reg_top_ptr: std::ptr::null_mut(),
            sync_reentry_depth_ptr: std::ptr::null_mut(),
            sync_reentry_limit: 0,
            array_index_accessor_protector_ptr: &array_index_accessor_protector,
            collection_method_ics: std::ptr::null(),
            collection_method_ic_count: 0,
            direct_method_inline: std::ptr::null(),
            gc_heap: std::ptr::null(),
            interrupt_flag: &interrupt_probe,
            backedge_fuel: &mut backedge_fuel_probe,
        };
        // SAFETY: integer-only function; never dereferences the null vm/stack.
        let entry: JitEntry = unsafe { std::mem::transmute(code.entry_ptr_for_test()) };
        let JitRet { value, status } = entry(&mut ctx);
        if status == STATUS_RETURNED {
            Exit::Returned(value)
        } else {
            Exit::Bailed
        }
    }

    fn expect_int(view: &JitCompileSnapshot, regs: &mut [u64], expected: i32) {
        match run(view, regs) {
            Exit::Returned(bits) => assert_eq!(unbox_i32(bits), expected),
            Exit::Bailed => panic!("expected Returned({expected}), got Bailed"),
        }
    }

    fn expect_f64(view: &JitCompileSnapshot, regs: &mut [u64], expected: f64) {
        match run(view, regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), expected),
            Exit::Bailed => panic!("expected Returned({expected}), got Bailed"),
        }
    }

    #[test]
    fn add_two_ints() {
        let v = view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(10), box_i32(20), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 30);
    }

    #[test]
    fn immediate_load_and_sub() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(100)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(1), Operand::Imm32(42)],
            ),
            (
                Op::Sub,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [0u64; 8];
        expect_int(&v, &mut regs, 58);
    }

    #[test]
    fn bitwise_or_truncates_in_range_double() {
        let v = view(&[
            (
                Op::BitwiseOr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(123.9), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 123);
    }

    #[test]
    fn bitwise_or_wraps_out_of_range_double_mod_pow2_32() {
        // A finite double past the signed-32-bit range is the full ECMAScript
        // `ToInt32`: truncate toward zero, reduce mod 2^32 into the signed
        // range. `2^31 | 0 == -2^31`, `2^32 + 5 | 0 == 5`. These come up
        // whenever an int arithmetic result overflows int32 into a double and
        // is then masked with `| 0`, so they must stay compiled, not bail.
        let v = view(&[
            (
                Op::BitwiseOr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(2_147_483_648.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, -2_147_483_648);
        let mut regs = [box_f64(4_294_967_301.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 5);
        let mut regs = [box_f64(-2_147_483_649.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 2_147_483_647);
    }

    #[test]
    fn bitwise_or_bails_on_non_finite_double() {
        // Infinity / NaN / `|x| >= 2^63` would saturate the 64-bit `fcvtzs`, so
        // they bail to the interpreter for exact coercion (`ToInt32` of each is
        // `0`).
        let v = view(&[
            (
                Op::BitwiseOr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(f64::INFINITY), box_i32(0), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
        let mut regs = [
            box_f64(9_223_372_036_854_775_808.0),
            box_i32(0),
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn ushr_boxes_unsigned_int32_result_as_double() {
        let v = view(&[
            (
                Op::Ushr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(-1), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_f64(&v, &mut regs, 4_294_967_295.0);
    }

    #[test]
    fn ushr_truncates_positive_double_mod_uint32() {
        let v = view(&[
            (
                Op::Ushr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(4_294_967_301.9), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_f64(&v, &mut regs, 5.0);
    }

    #[test]
    fn ushr_wraps_negative_double_mod_uint32() {
        // `ToUint32` of a negative finite double wraps mod 2^32: `-1 >>> 0`
        // is `4294967295`, not a bail.
        let v = view(&[
            (
                Op::Ushr,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(-1.0), box_i32(0), 0, 0, 0, 0, 0, 0];
        expect_f64(&v, &mut regs, 4_294_967_295.0);
    }

    #[test]
    fn negative_immediate_roundtrips() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(-7)],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let mut regs = [0u64; 8];
        expect_int(&v, &mut regs, -7);
    }

    #[test]
    fn counted_loop_sums_one_to_n() {
        // r0=n; sum=r1, i=r2, one=r4, cond=r3
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(1)]),
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)]),
            (
                Op::LessEq,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(rel(4, 8)), Operand::Register(3)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Register(4),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(rel(7, 3))]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        for (n, expected) in [(0, 0), (1, 1), (5, 15), (10, 55), (100, 5050)] {
            let mut regs = [box_i32(n), 0, 0, 0, 0, 0, 0, 0];
            expect_int(&v, &mut regs, expected);
        }
    }

    #[test]
    fn less_than_produces_boolean() {
        let v = view(&[
            (
                Op::LessThan,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let true_bits = VALUE_TRUE;
        let false_bits = VALUE_FALSE;
        let mut regs = [box_i32(3), box_i32(9), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Returned(b) if b == true_bits));
        let mut regs = [box_i32(9), box_i32(3), 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Returned(b) if b == false_bits));
    }

    fn box_f64(v: f64) -> u64 {
        let bits = if v.is_nan() {
            value_tag::CANONICAL_NAN
        } else {
            v.to_bits()
        };
        value_tag::box_double(bits)
    }
    fn unbox_f64(bits: u64) -> f64 {
        f64::from_bits(value_tag::unbox_double(bits))
    }
    fn add_view() -> JitCompileSnapshot {
        view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ])
    }

    #[test]
    fn float_comparisons_including_nan() {
        let t = VALUE_TRUE;
        let f = VALUE_FALSE;
        let cmp_view = |op: Op| {
            view(&[
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ])
        };
        let run_cmp = |op: Op, a: u64, b: u64| {
            let v = cmp_view(op);
            let mut regs = [a, b, 0, 0, 0, 0, 0, 0];
            match run(&v, &mut regs) {
                Exit::Returned(bits) => bits,
                Exit::Bailed => panic!("cmp bailed"),
            }
        };
        // ordered doubles
        assert_eq!(run_cmp(Op::LessThan, box_f64(1.5), box_f64(2.5)), t);
        assert_eq!(run_cmp(Op::LessThan, box_f64(2.5), box_f64(1.5)), f);
        assert_eq!(run_cmp(Op::LessEq, box_f64(2.5), box_f64(2.5)), t);
        assert_eq!(run_cmp(Op::GreaterThan, box_f64(3.0), box_f64(2.0)), t);
        assert_eq!(run_cmp(Op::Equal, box_f64(2.0), box_f64(2.0)), t);
        // mixed int/double
        assert_eq!(run_cmp(Op::LessThan, box_i32(1), box_f64(2.5)), t);
        assert_eq!(run_cmp(Op::GreaterEq, box_f64(4.0), box_i32(4)), t);
        // NaN: every relational compare is false, `!=` is true.
        let nan = box_f64(f64::NAN);
        assert_eq!(run_cmp(Op::LessThan, nan, box_f64(1.0)), f);
        assert_eq!(run_cmp(Op::LessEq, nan, box_f64(1.0)), f);
        assert_eq!(run_cmp(Op::GreaterThan, nan, box_f64(1.0)), f);
        assert_eq!(run_cmp(Op::Equal, nan, nan), f);
        assert_eq!(run_cmp(Op::NotEqual, nan, box_f64(1.0)), t);
    }

    #[test]
    fn strict_non_number_identity_comparisons() {
        let t = VALUE_TRUE;
        let f = VALUE_FALSE;
        let cmp_view = |op: Op| {
            view(&[
                (
                    op,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(2)]),
            ])
        };
        let run_cmp = |op: Op, a: u64, b: u64| {
            let v = cmp_view(op);
            let mut regs = [a, b, 0, 0, 0, 0, 0, 0];
            match run(&v, &mut regs) {
                Exit::Returned(bits) => Some(bits),
                Exit::Bailed => None,
            }
        };
        // Non-number immediates (here, booleans) decide identity inline by raw
        // bit comparison.
        assert_eq!(run_cmp(Op::Equal, t, t), Some(t));
        assert_eq!(run_cmp(Op::Equal, t, f), Some(f));
        assert_eq!(run_cmp(Op::NotEqual, t, f), Some(t));
        // Heap cells (objects, strings, BigInts) bail to the interpreter, which
        // owns object identity and string / BigInt content equality.
        let obj_a = 0x1234; // bare cage offset = heap cell
        let obj_b = 0x5678;
        assert_eq!(run_cmp(Op::Equal, obj_a, obj_b), None);
        assert_eq!(run_cmp(Op::Equal, obj_a, VALUE_NULL), None);
    }

    #[test]
    fn bails_on_non_number_operand() {
        // A tagged non-number (undefined = TAG_SPECIAL, payload 0) must bail to
        // the interpreter for numeric-only operators; only int32 and doubles
        // take the compiled arith path. `Add` has a runtime fallback for JS
        // string/primitive concatenation semantics.
        let v = view(&[
            (
                Op::Sub,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(10), VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn to_primitive_bails_on_heap_cell() {
        // A heap cell (object, callable, string) bails to the interpreter so any
        // observable `@@toPrimitive` / `valueOf` / `toString` still runs; the
        // value word alone cannot tell an already-primitive string from an
        // object that needs coercion.
        let v = view(&[
            (
                Op::ToPrimitive,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let cell = 0x1234;
        let mut regs = [cell, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn adds_two_doubles() {
        let v = add_view();
        let mut regs = [box_f64(1.5), box_f64(2.25), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.75),
            Exit::Bailed => panic!("expected 3.75, bailed"),
        }
    }

    #[test]
    fn mixes_int_and_double() {
        // int32(10) + double(2.5) → double(12.5): the int operand sign-converts.
        let v = add_view();
        let mut regs = [box_i32(10), box_f64(2.5), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 12.5),
            Exit::Bailed => panic!("expected 12.5, bailed"),
        }
    }

    #[test]
    fn divides_doubles_and_ints() {
        let v = view(&[
            (
                Op::Div,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_f64(7.0), box_f64(2.0), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.5),
            Exit::Bailed => panic!("expected 3.5, bailed"),
        }
        // 6 / 2 yields the Number 3 (an f64), not an int32.
        let mut regs = [box_i32(6), box_i32(2), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.0),
            Exit::Bailed => panic!("expected 3.0, bailed"),
        }
    }

    #[test]
    fn to_numeric_passes_double_through() {
        let v = view(&[
            (
                Op::ToNumeric,
                vec![Operand::Register(1), Operand::Register(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let mut regs = [box_f64(2.5), 0, 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 2.5),
            Exit::Bailed => panic!("expected 2.5, bailed"),
        }
        // A non-number (undefined) still bails.
        let mut regs = [VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed));
    }

    #[test]
    fn increment_int_double_and_overflow() {
        let v = view(&[
            (
                Op::Increment,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        // int + 1 stays int32.
        let mut regs = [box_i32(41), 0, 0, 0, 0, 0, 0, 0];
        expect_int(&v, &mut regs, 42);
        // double + 1 stays double.
        let mut regs = [box_f64(2.5), 0, 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.5),
            Exit::Bailed => panic!("expected 3.5, bailed"),
        }
        // i32::MAX + 1 overflows → exact double.
        let mut regs = [box_i32(i32::MAX), 0, 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), i32::MAX as f64 + 1.0),
            Exit::Bailed => panic!("expected overflow→double, bailed"),
        }
        // Decrement (delta = -1).
        let vd = view(&[
            (
                Op::Increment,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(-1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let mut regs = [box_i32(10), 0, 0, 0, 0, 0, 0, 0];
        expect_int(&vd, &mut regs, 9);
    }

    #[test]
    fn int_multiply_overflow_promotes_to_double() {
        // 100000 * 100000 = 1e10 overflows i32; the result is its exact f64
        // value via the double path, not a bail.
        let v = view(&[
            (
                Op::Mul,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let mut regs = [box_i32(100_000), box_i32(100_000), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 1e10),
            Exit::Bailed => panic!("expected 1e10, bailed"),
        }
    }

    #[test]
    fn unsupported_call_arg_overflow_reports_err() {
        // argc beyond MAX_INLINE_ARGS → Unsupported (not a compile success).
        let v = view(&[(
            Op::Call,
            vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(8),
                Operand::Register(2),
                Operand::Register(3),
                Operand::Register(4),
                Operand::Register(5),
                Operand::Register(6),
                Operand::Register(7),
                Operand::Register(8),
                Operand::Register(9),
            ],
        )]);
        assert!(compile(&v).is_err());
    }

    #[test]
    fn lowering_plan_rejects_non_boundary_branch_target() {
        let v = view(&[
            (Op::Jump, vec![Operand::Imm32(8)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert_eq!(
            BaselinePlan::build(&v).err(),
            Some(Unsupported::BranchTarget(9))
        );
    }

    #[test]
    fn lowering_plan_publishes_canonical_branch_target() {
        let v = view(&[
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");
        assert_eq!(plan.branch_target(0), Ok(1));
    }

    #[test]
    fn lowering_plan_assigns_allocating_add_safepoint() {
        let v = view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");
        let id = plan
            .add_alloc_safepoints
            .get(&0)
            .copied()
            .expect("add safepoint");
        assert!(plan.safepoint_records.iter().any(|record| record.id == id));
    }

    #[test]
    fn method_call_uses_full_packed_argument_abi() {
        let four_args = view(&[
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(4),
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::Register(4),
                    Operand::Register(5),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(
            compile(&four_args).is_ok(),
            "the baseline must accept every argument representable by the shared packed ABI"
        );

        let five_args = view(&[
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(5),
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::Register(4),
                    Operand::Register(5),
                    Operand::Register(6),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(compile(&five_args).is_err());
    }

    #[test]
    fn store_element_is_part_of_the_baseline_subset() {
        let store = view(&[
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(
            compile(&store).is_ok(),
            "the emitted dense/typed-array fast path and typed runtime miss must be reachable"
        );
    }
}
