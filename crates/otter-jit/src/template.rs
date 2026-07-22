//! Template compiler built on backend-neutral [`TemplatePlan`] operations.
//!
//! The production native compiler over the frozen entry contract
//! (`JitCtx`/`JitRet`, `otter_vm::native_abi`). It compiles constants,
//! register moves, branches, tagged truthiness, the full tagged
//! numeric/comparison/bitwise set, `+` with allocating string concat,
//! descriptor-resolved runtime transitions, ordinary and method calls with
//! callee-owned frames, named-property IC probes, and guarded Map/Set builtin
//! fast paths. Opcodes that are safe to retry before any observable effect
//! lower to canonical-PC exact side exits (loop-OSR-only code). Structured
//! exception regions complete through the VM's canonical handler/unwind
//! implementation and return committed same-frame continuations.
//!
//! # Contents
//! - [`plan`] — machine-independent operation stream over typed lowering.
//! - [`inline_leaf`] — deopt-safe call/method leaf validation and compact
//!   scratch planning.
//! - [`arm64`] — the AArch64 dynasm backend (first machine target).
//! - [`code`] — finalized [`TemplateCode`] objects and VM entry publication.
//! - [`compile`] — the whole-function compile entry point.
//!
//! # Invariants
//! - Compiled code publishes the canonical instruction-index PC before every
//!   operation capable of a side exit, runtime transition, or back-edge poll.
//!   Pure constants, moves, forward control flow, and returns cannot exit, so
//!   they inherit the last publication. Every actual interpreter resume still
//!   names the exact operation and never replays a committed effect.
//! - The VM is reached only through `otter_vm::native_abi` records and the
//!   shared runtime-stub inventory; no template-private frame or status shape
//!   exists.
//!
//! # See also
//! - `OTTER_PLAN.md` — active engine direction and verification gates.
//! - [`crate::entry`] — the shared entry ABI, typed lowering, and runtime
//!   transitions this compiler consumes.

use otter_vm::JitCompileSnapshot;

#[cfg(target_arch = "aarch64")]
pub(crate) mod arm64;
mod code;
mod inline_leaf;
mod plan;

pub use code::TemplateCode;
pub(crate) use inline_leaf::{InlineEntryValue, InlineLeafPlan, InlineScratchSlot};
pub(crate) use plan::{
    ArithKind, BitwiseKind, CompareKind, TemplateOp, TemplatePlan, TemplateTail,
};

use crate::entry::{TransitionTable, Unsupported};

/// Compile a function view to template machine code under the
/// isolate-assigned unique code-object identity, or report why not. The
/// caller provides the hook-lifetime [`TransitionTable`] so per-compile work
/// stays plan-and-emit only.
#[cfg(target_arch = "aarch64")]
pub fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
) -> Result<TemplateCode, Unsupported> {
    arm64::compile(view, code_object_id, transitions, None, false).map(|output| output.code)
}

/// Compile with an optional default-off artifact sidecar.
#[cfg(target_arch = "aarch64")]
pub(crate) fn compile_with_artifacts(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
    artifact_request: Option<crate::artifact::ArtifactRequest>,
    capture_events: bool,
) -> Result<crate::artifact::NativeCompileOutput<TemplateCode>, Unsupported> {
    arm64::compile(
        view,
        code_object_id,
        transitions,
        artifact_request,
        capture_events,
    )
}

/// Non-arm64 stub: the template backend is arm64-only for now.
#[cfg(not(target_arch = "aarch64"))]
pub fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
    transitions: &TransitionTable,
) -> Result<TemplateCode, Unsupported> {
    let _ = (view, code_object_id, transitions);
    Err(Unsupported::OperandShape("template compiler is arm64-only"))
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    //! Execution tests for the template subset. They drive compiled code
    //! through a `JitCtx` whose `vm`/`stack`/`context` are null — valid
    //! because this subset never re-enters the VM — and a `regs` pointer at a
    //! local register array.

    use super::TemplateCode;
    use crate::entry::{
        JitCtx, JitEntry, JitRet, STATUS_RETURNED, VALUE_FALSE, VALUE_HOLE, VALUE_NULL, VALUE_TRUE,
        VALUE_UNDEFINED, value_tag,
    };
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, JitFunctionCode, jit::JitTestInstruction};

    const STRIDE: u32 = 4;

    fn box_i32(v: i32) -> u64 {
        value_tag::box_int32(v)
    }
    fn box_f64(v: f64) -> u64 {
        let bits = if v.is_nan() {
            value_tag::CANONICAL_NAN
        } else {
            v.to_bits()
        };
        value_tag::box_double(bits)
    }
    fn unbox_i32(bits: u64) -> i32 {
        value_tag::unbox_int32(bits)
    }

    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitCompileSnapshot {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| {
                JitTestInstruction::new(*op, idx as u32, idx as u32 * STRIDE, operands.clone())
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, 1, 8, instructions)
    }

    // CodeBlock branch encoding: target instruction = current + 1 + rel.
    fn rel(from: usize, to: usize) -> i32 {
        to as i32 - from as i32 - 1
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Exit {
        Returned(u64),
        Bailed(u32),
    }

    /// Drive one compiled entry with explicit boundary probes: the interrupt
    /// byte and the back-edge fuel budget.
    fn exec_entry(entry_ptr: *const u8, regs: &mut [u64], interrupt: u8, fuel: u64) -> Exit {
        let mut error = None;
        let interrupt_probe: u8 = interrupt;
        let mut backedge_fuel_probe: u64 = fuel;
        let mut activation_probe = [0u64; 32];
        let mut activation_top_probe: usize = 0;
        let mut native_frame = otter_vm::native_abi::NativeFrame::new(
            otter_vm::native_abi::VmFrameHeader::interpreter(0, regs.len() as u16),
            regs.as_mut_ptr() as u64,
            otter_vm::Value::undefined(),
            otter_vm::Value::undefined(),
        );
        native_frame.set_materialized_activation(0);
        let mut thread = otter_vm::native_abi::VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.current_code_object_id = 1;
        thread.interrupt_cell = std::ptr::addr_of!(interrupt_probe) as u64;
        thread.backedge_fuel_cell = std::ptr::addr_of_mut!(backedge_fuel_probe) as u64;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: &mut native_frame,
            error: &mut error,
            activation_base: activation_probe.as_mut_ptr().cast(),
            activation_top_ptr: std::ptr::addr_of_mut!(activation_top_probe),
            activation_limit: 16,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
        };
        // SAFETY: subset code never re-enters the VM; the entry was emitted
        // with the shared compiled-entry ABI and its mapping outlives the call.
        let entry: JitEntry = unsafe { std::mem::transmute(entry_ptr) };
        let JitRet { value, status } = entry(&mut ctx);
        if status == STATUS_RETURNED {
            Exit::Returned(value)
        } else {
            Exit::Bailed(native_frame.header.pc)
        }
    }

    fn compile(view: &JitCompileSnapshot) -> Result<TemplateCode, super::Unsupported> {
        super::compile(view, 1, &crate::entry::TransitionTable::resolve())
    }

    fn run(view: &JitCompileSnapshot, regs: &mut [u64]) -> Exit {
        exec(view, regs, 0, 1 << 30)
    }

    fn exec(view: &JitCompileSnapshot, regs: &mut [u64], interrupt: u8, fuel: u64) -> Exit {
        let code = compile(view).expect("template compiles");
        // SAFETY: `code` stays alive for the complete native call.
        exec_entry(unsafe { code.entry_ptr_for_test() }, regs, interrupt, fuel)
    }

    #[test]
    fn constants_and_moves_round_trip() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(-7)],
            ),
            (Op::LoadTrue, vec![Operand::Register(1)]),
            (Op::LoadNull, vec![Operand::Register(2)]),
            (Op::LoadUndefined, vec![Operand::Register(3)]),
            (
                Op::StoreLocal,
                vec![Operand::Register(0), Operand::Imm32(4)],
            ),
            (Op::LoadLocal, vec![Operand::Register(5), Operand::Imm32(4)]),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ]);
        let mut regs = [0u64; 8];
        assert_eq!(run(&v, &mut regs), Exit::Returned(box_i32(-7)));
        assert_eq!(regs[1], VALUE_TRUE);
        assert_eq!(regs[2], VALUE_NULL);
        assert_eq!(regs[3], VALUE_UNDEFINED);
        assert_eq!(regs[4], box_i32(-7));
    }

    #[test]
    fn returns_undefined_without_a_source_register() {
        let v = view(&[(Op::ReturnUndefined, vec![])]);
        let mut regs = [0u64; 8];
        assert_eq!(run(&v, &mut regs), Exit::Returned(VALUE_UNDEFINED));
    }

    /// Countdown loop over a conditional back edge:
    /// `while (r0) { r0 = r1 }` shapes exercising branch + truthiness.
    fn countdown_view() -> JitCompileSnapshot {
        // r0 = n (int32); r1 accumulates by moving r2 (0) into r0 once.
        view(&[
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(rel(0, 3)), Operand::Register(0)],
            ),
            (
                Op::StoreLocal,
                vec![Operand::Register(1), Operand::Imm32(0)],
            ),
            (Op::Jump, vec![Operand::Imm32(rel(2, 0))]),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ])
    }

    #[test]
    fn conditional_back_edge_terminates_on_falsy_int() {
        let v = countdown_view();
        let mut regs = [box_i32(5), box_i32(0), 0, 0, 0, 0, 0, 0];
        assert_eq!(run(&v, &mut regs), Exit::Returned(box_i32(0)));
    }

    #[test]
    fn truthiness_decides_numbers_and_immediates_inline() {
        let v = view(&[
            (
                Op::ToBoolean,
                vec![Operand::Register(1), Operand::Register(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let cases = [
            (box_i32(0), VALUE_FALSE),
            (box_i32(-1), VALUE_TRUE),
            (box_f64(0.0), VALUE_FALSE),
            (box_f64(-0.0), VALUE_FALSE),
            (box_f64(f64::NAN), VALUE_FALSE),
            (box_f64(2.5), VALUE_TRUE),
            (box_f64(-2.5), VALUE_TRUE),
            (VALUE_TRUE, VALUE_TRUE),
            (VALUE_FALSE, VALUE_FALSE),
            (VALUE_NULL, VALUE_FALSE),
            (VALUE_UNDEFINED, VALUE_FALSE),
        ];
        for (input, expected) in cases {
            let mut regs = [input, 0, 0, 0, 0, 0, 0, 0];
            assert_eq!(
                run(&v, &mut regs),
                Exit::Returned(expected),
                "ToBoolean({input:#x})"
            );
        }
    }

    #[test]
    fn logical_not_inverts_the_inline_truthiness() {
        let v = view(&[
            (
                Op::LogicalNot,
                vec![Operand::Register(1), Operand::Register(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        let mut regs = [box_i32(0), 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(run(&v, &mut regs), Exit::Returned(VALUE_TRUE));
        let mut regs = [box_f64(2.5), 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(run(&v, &mut regs), Exit::Returned(VALUE_FALSE));
    }

    #[test]
    fn heap_cell_truthiness_side_exits_at_the_exact_pc() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(1), Operand::Imm32(41)],
            ),
            (
                Op::ToBoolean,
                vec![Operand::Register(2), Operand::Register(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        for cell_like in [0x1234u64, VALUE_HOLE, value_tag::box_function_id(3)] {
            let mut regs = [cell_like, 0, 0, 0, 0, 0, 0, 0];
            assert_eq!(run(&v, &mut regs), Exit::Bailed(1), "input {cell_like:#x}");
            assert_eq!(regs[1], box_i32(41), "earlier instruction stays committed");
            assert_eq!(regs[2], 0, "side-exit instruction remains uncommitted");
        }
    }

    #[test]
    fn interrupt_and_fuel_polls_carry_no_partial_effects() {
        // Every back edge takes the slow poll path (exhausted fuel, then a set
        // interrupt byte). The null-`vm` poll treats both as "continue", so the
        // loop must still complete with the same result.
        let v = countdown_view();
        for (interrupt, fuel) in [(0u8, 1u64), (1u8, 1 << 30)] {
            let mut regs = [box_i32(5), box_i32(0), 0, 0, 0, 0, 0, 0];
            assert_eq!(
                exec(&v, &mut regs, interrupt, fuel),
                Exit::Returned(box_i32(0))
            );
        }
    }

    #[test]
    fn structured_exception_opcodes_compile_for_entry() {
        let v = view(&[
            (Op::Throw, vec![Operand::Register(2)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let code = compile(&v).expect("Throw compiles through the exception transition");
        assert!(!code.osr_only());
    }

    #[test]
    fn code_size_stays_within_the_skeleton_budget() {
        let code = compile(&countdown_view()).expect("template compiles");
        assert!(code.code_len() > 0);
        assert!(
            code.code_len() < 2048,
            "countdown fixture grew to {} bytes",
            code.code_len()
        );
    }

    #[test]
    fn metadata_publishes_the_code_object_shape() {
        let code = compile(&countdown_view()).expect("template compiles");
        let metadata = code.metadata();
        assert_eq!(metadata.safepoint_count, 0);
        assert_eq!(code.safepoint_count(), 0);
        assert!(!code.osr_only());
    }

    #[test]
    fn artifact_capture_preserves_exact_code_and_layout() {
        let view = countdown_view();
        let transitions = crate::entry::TransitionTable::resolve();
        let without_capture = super::compile_with_artifacts(&view, 1, &transitions, None, false)
            .expect("template compiles without artifact capture");
        let with_capture = super::compile_with_artifacts(
            &view,
            1,
            &transitions,
            Some(crate::artifact::ArtifactRequest {
                identity: otter_vm::JitArtifactIdentity {
                    function_name: "countdown".to_string(),
                    module: "artifact-code-identity.js".to_string(),
                },
                tier: otter_vm::JitDebugTier::Template,
                entry: otter_vm::JitDebugTarget::Entry,
            }),
            false,
        )
        .expect("template compiles with artifact capture");

        assert_eq!(
            without_capture.code.exact_bytes_for_test(),
            with_capture.code.exact_bytes_for_test(),
            "default-off capture must not change executable bytes"
        );
        assert_eq!(
            without_capture.code.metadata(),
            with_capture.code.metadata(),
            "capture must not change entry offset or code-object shape"
        );
        assert_eq!(
            without_capture.code.osr_entries_for_test(),
            with_capture.code.osr_entries_for_test(),
            "capture must not move OSR entry trampolines"
        );

        let artifact = with_capture
            .artifact
            .expect("enabled capture returns an artifact");
        let captured_code = artifact
            .file(otter_vm::JitArtifactFileName::Code)
            .expect("artifact contains exact code.bin");
        assert_eq!(
            captured_code.contents(),
            with_capture.code.exact_bytes_for_test(),
            "code.bin must be the exact installed mapping"
        );
    }

    /// Executable fixtures spanning constants, branches, moves, and returns,
    /// executed through the shared entry ABI; each result must match the
    /// expected interpreter semantics.
    #[test]
    fn shared_fixtures_match_interpreter_semantics() {
        let fixtures: Vec<(&str, JitCompileSnapshot, [u64; 8], Exit)> = vec![
            (
                "int constant return",
                view(&[
                    (
                        Op::LoadInt32,
                        vec![Operand::Register(0), Operand::Imm32(42)],
                    ),
                    (Op::ReturnValue, vec![Operand::Register(0)]),
                ]),
                [0; 8],
                Exit::Returned(box_i32(42)),
            ),
            (
                "boolean branch select",
                view(&[
                    (
                        Op::JumpIfFalse,
                        vec![Operand::Imm32(rel(0, 3)), Operand::Register(0)],
                    ),
                    (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(1)]),
                    (Op::Jump, vec![Operand::Imm32(rel(2, 4))]),
                    (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
                    (Op::ReturnValue, vec![Operand::Register(1)]),
                ]),
                [VALUE_TRUE, 0, 0, 0, 0, 0, 0, 0],
                Exit::Returned(box_i32(1)),
            ),
            (
                "window move return",
                view(&[
                    (
                        Op::StoreLocal,
                        vec![Operand::Register(0), Operand::Imm32(3)],
                    ),
                    (Op::LoadLocal, vec![Operand::Register(1), Operand::Imm32(3)]),
                    (Op::ReturnValue, vec![Operand::Register(1)]),
                ]),
                [box_i32(9), 0, 0, 0, 0, 0, 0, 0],
                Exit::Returned(box_i32(9)),
            ),
            (
                "undefined return",
                view(&[(Op::ReturnUndefined, vec![])]),
                [0; 8],
                Exit::Returned(VALUE_UNDEFINED),
            ),
        ];
        for (name, v, regs_in, expected) in fixtures {
            let template_code = compile(&v).expect("template compiles the shared fixture");
            let mut template_regs = regs_in;
            // SAFETY: the code object outlives the call.
            let template_exit = exec_entry(
                unsafe { template_code.entry_ptr_for_test() },
                &mut template_regs,
                0,
                1 << 30,
            );
            assert_eq!(template_exit, expected, "template fixture `{name}`");
        }
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
            "the compiler must accept every argument representable by the shared packed ABI"
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
        assert!(
            compile(&five_args).is_ok(),
            "argument lists beyond the inline lanes spill into the code object's register table"
        );
    }

    #[test]
    fn store_element_is_part_of_the_compiled_subset() {
        let store = view(&[
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        assert!(
            compile(&store).is_ok(),
            "element stores must stay compiled through the typed runtime transition"
        );
    }

    #[test]
    fn unboxes_are_consistent_with_the_vm_encoding() {
        assert_eq!(unbox_i32(box_i32(-7)), -7);
    }

    fn unbox_f64(bits: u64) -> f64 {
        f64::from_bits(value_tag::unbox_double(bits))
    }

    fn binary_view(op: Op) -> JitCompileSnapshot {
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
    }

    fn run_binary(op: Op, lhs: u64, rhs: u64) -> Exit {
        let v = binary_view(op);
        let mut regs = [lhs, rhs, 0, 0, 0, 0, 0, 0];
        run(&v, &mut regs)
    }

    fn expect_binary_int(op: Op, lhs: u64, rhs: u64, expected: i32) {
        match run_binary(op, lhs, rhs) {
            Exit::Returned(bits) => assert_eq!(unbox_i32(bits), expected, "{op:?}"),
            Exit::Bailed(pc) => panic!("{op:?} bailed at {pc}"),
        }
    }

    fn expect_binary_f64(op: Op, lhs: u64, rhs: u64, expected: f64) {
        match run_binary(op, lhs, rhs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), expected, "{op:?}"),
            Exit::Bailed(pc) => panic!("{op:?} bailed at {pc}"),
        }
    }

    #[test]
    fn arithmetic_int_double_and_overflow_paths() {
        expect_binary_int(Op::Add, box_i32(10), box_i32(20), 30);
        expect_binary_int(Op::Sub, box_i32(10), box_i32(42), -32);
        expect_binary_int(Op::Mul, box_i32(-6), box_i32(7), -42);
        expect_binary_f64(Op::Add, box_f64(1.5), box_f64(2.25), 3.75);
        expect_binary_f64(Op::Add, box_i32(10), box_f64(2.5), 12.5);
        expect_binary_f64(
            Op::Add,
            box_i32(i32::MAX),
            box_i32(1),
            i32::MAX as f64 + 1.0,
        );
        expect_binary_f64(
            Op::Mul,
            box_i32(100_000),
            box_i32(100_000),
            10_000_000_000.0,
        );
        expect_binary_f64(Op::Div, box_i32(6), box_i32(2), 3.0);
        expect_binary_f64(Op::Div, box_f64(7.0), box_f64(2.0), 3.5);
        expect_binary_int(Op::Rem, box_i32(7), box_i32(3), 1);
        expect_binary_int(Op::Rem, box_i32(-7), box_i32(3), -1);
        expect_binary_int(Op::Rem, box_i32(6), box_i32(3), 0);
        // Cases int32 cannot represent complete through the leaf f64
        // remainder probe: NaN for a zero divisor, `-0` for a zero
        // remainder of a negative dividend, and double operands.
        match run_binary(Op::Rem, box_i32(7), box_i32(0)) {
            Exit::Returned(bits) => assert!(unbox_f64(bits).is_nan()),
            Exit::Bailed(pc) => panic!("zero-divisor remainder bailed at {pc}"),
        }
        match run_binary(Op::Rem, box_i32(-6), box_i32(3)) {
            Exit::Returned(bits) => {
                assert_eq!(unbox_f64(bits), 0.0);
                assert!(unbox_f64(bits).is_sign_negative(), "-0 must stay signed");
            }
            Exit::Bailed(pc) => panic!("-0 remainder bailed at {pc}"),
        }
        expect_binary_f64(Op::Rem, box_f64(7.5), box_i32(2), 1.5);
        // Non-number operands side-exit for exact coercion.
        assert!(matches!(
            run_binary(Op::Sub, box_i32(1), VALUE_UNDEFINED),
            Exit::Bailed(_)
        ));
    }

    #[test]
    fn bitwise_full_to_int32_semantics() {
        expect_binary_int(Op::BitwiseOr, box_f64(123.9), box_i32(0), 123);
        expect_binary_int(
            Op::BitwiseOr,
            box_f64(2_147_483_648.0),
            box_i32(0),
            i32::MIN,
        );
        expect_binary_int(Op::BitwiseOr, box_f64(4_294_967_301.0), box_i32(0), 5);
        expect_binary_int(Op::BitwiseAnd, box_i32(0b1100), box_i32(0b1010), 0b1000);
        expect_binary_int(Op::BitwiseXor, box_i32(0b1100), box_i32(0b1010), 0b0110);
        expect_binary_int(Op::Shl, box_i32(1), box_i32(33), 2);
        expect_binary_int(Op::Shr, box_i32(-8), box_i32(1), -4);
        expect_binary_f64(Op::Ushr, box_i32(-1), box_i32(0), 4_294_967_295.0);
        expect_binary_f64(Op::Ushr, box_f64(-1.0), box_i32(0), 4_294_967_295.0);
        // Non-finite doubles saturate fcvtzs → exact side exit.
        assert!(matches!(
            run_binary(Op::BitwiseOr, box_f64(f64::INFINITY), box_i32(0)),
            Exit::Bailed(_)
        ));
        assert!(matches!(
            run_binary(Op::BitwiseOr, box_f64(f64::NAN), box_i32(0)),
            Exit::Bailed(_)
        ));
    }

    #[test]
    fn comparisons_including_nan_and_strict_identity() {
        let t = VALUE_TRUE;
        let f = VALUE_FALSE;
        let expect = |op: Op, lhs: u64, rhs: u64, expected: u64| match run_binary(op, lhs, rhs) {
            Exit::Returned(bits) => assert_eq!(bits, expected, "{op:?}"),
            Exit::Bailed(pc) => panic!("{op:?} bailed at {pc}"),
        };
        expect(Op::LessThan, box_i32(3), box_i32(9), t);
        expect(Op::LessThan, box_f64(2.5), box_f64(1.5), f);
        expect(Op::LessEq, box_f64(2.5), box_f64(2.5), t);
        expect(Op::GreaterThan, box_i32(9), box_i32(3), t);
        expect(Op::GreaterEq, box_f64(4.0), box_i32(4), t);
        let nan = box_f64(f64::NAN);
        expect(Op::LessThan, nan, box_f64(1.0), f);
        expect(Op::Equal, nan, nan, f);
        expect(Op::NotEqual, nan, box_f64(1.0), t);
        // Strict identity on non-number immediates.
        expect(Op::Equal, t, t, t);
        expect(Op::Equal, t, f, f);
        expect(Op::Equal, VALUE_NULL, VALUE_UNDEFINED, f);
        expect(Op::Equal, box_i32(1), VALUE_TRUE, f);
        // Identical heap-cell bits are the same cell — strict equality
        // decides inline without a probe.
        expect(Op::Equal, 0x1234, 0x1234, t);
        expect(Op::NotEqual, 0x1234, 0x1234, f);
        // Distinct cells ask the leaf content-equality probe; without a live
        // isolate heap (this harness) the probe misses and the site takes
        // the exact side exit.
        assert!(matches!(
            run_binary(Op::Equal, 0x1234, 0x5678),
            Exit::Bailed(_)
        ));
    }

    #[test]
    fn loose_equality_numbers_and_nullish() {
        let t = VALUE_TRUE;
        let f = VALUE_FALSE;
        let expect = |op: Op, lhs: u64, rhs: u64, expected: u64| match run_binary(op, lhs, rhs) {
            Exit::Returned(bits) => assert_eq!(bits, expected, "{op:?}"),
            Exit::Bailed(pc) => panic!("{op:?} bailed at {pc}"),
        };
        expect(Op::LooseEqual, VALUE_NULL, VALUE_UNDEFINED, t);
        expect(Op::LooseEqual, VALUE_NULL, box_i32(0), f);
        expect(Op::LooseEqual, box_i32(1), box_f64(1.0), t);
        expect(Op::LooseNotEqual, box_i32(1), box_i32(2), t);
        // Coercive cases (booleans, strings) side-exit.
        assert!(matches!(
            run_binary(Op::LooseEqual, VALUE_TRUE, box_i32(1)),
            Exit::Bailed(_)
        ));
    }

    #[test]
    fn increment_negate_and_conversions() {
        let increment = |delta: i32, input: u64| {
            let v = view(&[
                (
                    Op::Increment,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::Imm32(delta),
                    ],
                ),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ]);
            let mut regs = [input, 0, 0, 0, 0, 0, 0, 0];
            run(&v, &mut regs)
        };
        assert_eq!(increment(1, box_i32(41)), Exit::Returned(box_i32(42)));
        assert_eq!(increment(-1, box_i32(10)), Exit::Returned(box_i32(9)));
        match increment(1, box_i32(i32::MAX)) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), i32::MAX as f64 + 1.0),
            Exit::Bailed(pc) => panic!("overflow increment bailed at {pc}"),
        }
        match increment(1, box_f64(2.5)) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.5),
            Exit::Bailed(pc) => panic!("double increment bailed at {pc}"),
        }
        assert!(matches!(increment(1, VALUE_UNDEFINED), Exit::Bailed(_)));

        let unary = |op: Op, input: u64| {
            let v = view(&[
                (op, vec![Operand::Register(1), Operand::Register(0)]),
                (Op::ReturnValue, vec![Operand::Register(1)]),
            ]);
            let mut regs = [input, 0, 0, 0, 0, 0, 0, 0];
            run(&v, &mut regs)
        };
        assert_eq!(unary(Op::Neg, box_i32(42)), Exit::Returned(box_i32(-42)));
        match unary(Op::Neg, box_i32(0)) {
            Exit::Returned(bits) => {
                assert_eq!(unbox_f64(bits), 0.0);
                assert!(unbox_f64(bits).is_sign_negative(), "-0 must stay signed");
            }
            Exit::Bailed(pc) => panic!("-0 negate bailed at {pc}"),
        }
        match unary(Op::Neg, box_i32(i32::MIN)) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 2_147_483_648.0),
            Exit::Bailed(pc) => panic!("-i32::MIN negate bailed at {pc}"),
        }
        match unary(Op::Neg, box_f64(2.5)) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), -2.5),
            Exit::Bailed(pc) => panic!("double negate bailed at {pc}"),
        }
        assert!(matches!(unary(Op::Neg, VALUE_NULL), Exit::Bailed(_)));

        match unary(Op::ToNumeric, box_f64(2.5)) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 2.5),
            Exit::Bailed(pc) => panic!("ToNumeric bailed at {pc}"),
        }
        assert!(matches!(
            unary(Op::ToNumeric, VALUE_UNDEFINED),
            Exit::Bailed(_)
        ));
    }

    #[test]
    fn triangular_sum_loop_runs_fully_compiled() {
        // r0=n; sum=r1, i=r2, one=r4, cond=r3 — the arithmetic + branch mix
        // proving the numeric subset composes across a real loop.
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
            assert_eq!(run(&v, &mut regs), Exit::Returned(box_i32(expected)));
        }
    }
}
