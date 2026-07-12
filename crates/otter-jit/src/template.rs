//! Template compiler built on backend-neutral [`TemplatePlan`] operations.
//!
//! A second, deliberately small native compiler that lives beside the
//! established baseline emitter and shares its frozen entry contract
//! (`JitCtx`/`JitRet`, `otter_vm::native_abi`). It compiles exactly the
//! constants / register-move / branch / tagged-truthiness / return subset and
//! rejects every other opcode with a whole-function [`Unsupported`] — a
//! template compile never produces partially executable code.
//!
//! # Contents
//! - [`plan`] — machine-independent operation stream over typed lowering.
//! - [`arm64`] — the AArch64 dynasm backend (first machine target).
//! - [`code`] — finalized [`TemplateCode`] objects and VM entry publication.
//! - [`compile`] — the whole-function compile entry point.
//!
//! # Invariants
//! - Selection is an explicit code-level wiring point
//!   ([`crate::BaselineCompilerKind`]); there is no environment or runtime
//!   toggle, and the default hook construction never routes here.
//! - Compiled code publishes the canonical instruction-index PC before every
//!   opcode and exits only through exact side exits, returns, or the throw
//!   status — identical boundary semantics to the baseline emitter.
//! - The VM is reached only through `otter_vm::native_abi` records and the
//!   shared runtime-stub inventory; no template-private frame or status shape
//!   exists.
//!
//! # See also
//! - `JIT_REFACTOR_PLAN.md` — replacement-compiler direction and gates.
//! - [`crate::baseline`] — the full-subset emitter this compiler will replace.

use otter_vm::JitCompileSnapshot;

#[cfg(target_arch = "aarch64")]
mod arm64;
mod code;
mod plan;

pub use code::TemplateCode;
pub(crate) use plan::{TemplateOp, TemplatePlan};

use crate::baseline::Unsupported;

/// Compile a function view to template machine code under the
/// isolate-assigned unique code-object identity, or report why not.
#[cfg(target_arch = "aarch64")]
pub fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<TemplateCode, Unsupported> {
    arm64::compile(view, code_object_id)
}

/// Non-arm64 stub: the template backend is arm64-only for now.
#[cfg(not(target_arch = "aarch64"))]
pub fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<TemplateCode, Unsupported> {
    let _ = (view, code_object_id);
    Err(Unsupported::OperandShape("template compiler is arm64-only"))
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    //! Execution tests for the template subset, plus differential fixtures
    //! compiled by both the template and the baseline compiler. They drive
    //! compiled code through a `JitCtx` whose `vm`/`stack`/`context` are null —
    //! valid because this subset never re-enters the VM — and a `regs` pointer
    //! at a local register array.

    use super::TemplateCode;
    use crate::baseline::{
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
        let mut sync_reentry_depth_probe: u32 = 0;
        let mut reg_stack_probe = [0u64; 64];
        let mut reg_top_probe: usize = 0;
        let mut native_frame = otter_vm::native_abi::NativeFrame {
            header: otter_vm::native_abi::VmFrameHeader::interpreter(0, regs.len() as u16),
            previous_frame: 0,
            register_base: regs.as_mut_ptr() as u64,
            argument_base: 0,
            feedback_base: 0,
            code_object_id: 1,
            this_value_bits: 0,
            new_target_bits: 0,
            return_register: u32::MAX,
            cold_state_index: u32::MAX,
            argument_count: 0,
            reserved0: 0,
            feedback_id: 0,
        };
        let mut thread = otter_vm::native_abi::VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
        thread.interrupt_cell = std::ptr::addr_of!(interrupt_probe) as u64;
        thread.backedge_fuel_cell = std::ptr::addr_of_mut!(backedge_fuel_probe) as u64;
        thread.sync_reentry_depth_cell = std::ptr::addr_of_mut!(sync_reentry_depth_probe) as u64;
        thread.sync_reentry_limit = u32::MAX;
        let mut ctx = JitCtx {
            regs: regs.as_mut_ptr(),
            self_closure: 0,
            this_value: 0,
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: &mut native_frame,
            frame_index: 0,
            upvalues_ptr: 0,
            error: &mut error,
            direct_entry_addr: 0,
            direct_regs: std::ptr::null_mut(),
            direct_self_closure: 0,
            direct_this_value: 0,
            direct_frame_index: 0,
            direct_upvalues_ptr: 0,
            direct_frame_ids: 0,
            direct_frame_meta: 0,
            direct_code_object_id: 0,
            reg_stack_base: reg_stack_probe.as_mut_ptr(),
            reg_top_ptr: std::ptr::addr_of_mut!(reg_top_probe),
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
        super::compile(view, 1)
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
    fn unsupported_opcodes_reject_the_whole_function() {
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
        assert_eq!(compile(&v).err(), Some(super::Unsupported::Opcode(Op::Add)));
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
    fn metadata_publishes_a_compatible_code_object() {
        let code = compile(&countdown_view()).expect("template compiles");
        let metadata = code.metadata();
        assert!(metadata.is_compatible_with_current_vm());
        assert_eq!(metadata.safepoint_count, 0);
        assert_eq!(code.safepoint_count(), 0);
        assert!(!code.osr_only());
        assert!(!code.frameless_entry_safe());
    }

    /// Differential fixtures compiled by BOTH compilers and executed through
    /// the identical entry ABI. The template result must match the expected
    /// interpreter semantics; whenever the baseline compiler also completes
    /// (it bails on non-boolean branch conditions the template handles), the
    /// two compiled results must agree bit-for-bit.
    #[test]
    fn both_compilers_agree_on_shared_fixtures() {
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

            let baseline_code =
                crate::baseline::compile(&v, 2).expect("baseline compiles the shared fixture");
            let mut baseline_regs = regs_in;
            // SAFETY: the code object outlives the call.
            let baseline_exit = exec_entry(
                unsafe { baseline_entry_for_test(&baseline_code) },
                &mut baseline_regs,
                0,
                1 << 30,
            );
            if let Exit::Returned(_) = baseline_exit {
                assert_eq!(baseline_exit, template_exit, "A/B fixture `{name}`");
                assert_eq!(baseline_regs, template_regs, "A/B registers `{name}`");
            }
        }
    }

    // SAFETY-wrapper: reach the baseline entry through its VM entry address so
    // both compilers execute through the same test harness.
    unsafe fn baseline_entry_for_test(code: &crate::BaselineCode) -> *const u8 {
        code.entry_addr()
            .expect("baseline code publishes an entry address") as *const u8
    }

    #[test]
    fn unboxes_are_consistent_with_the_vm_encoding() {
        assert_eq!(unbox_i32(box_i32(-7)), -7);
    }
}
