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
//! - **Exact-PC side exit.** Before an opcode can guard, mutate, call, throw, or
//!   allocate, compiled code publishes that opcode's canonical instruction
//!   index in both its machine-local exit payload and the active native frame.
//!   `status: 1` means every earlier opcode is committed and the named opcode
//!   is uncommitted; the interpreter resumes exactly there and never replays
//!   earlier effects. Throws use `status: 2` and therefore never masquerade as
//!   resumable side exits.
//! - **Materialized roots.** Interpreter-visible registers remain in the
//!   published frame window at every side exit and allocating/reentrant call.
//!   A nested compiled call owns its machine-local PC until its frame is either
//!   completed or recovered by the direct-call tail.
//!
//! # See also
//! - `JIT_DESIGN.md` §3.2 (backend), §3.5 (GC contract), §4 Phase 1.

// dynasm 5 normalizes dynamic AArch64 register operands through `Into<u8>`;
// when our register ids are already `u8`, that macro-generated conversion is
// intentionally redundant and outside the source-level emitter's control.
#![allow(clippy::useless_conversion)]

use otter_bytecode::Op;
use otter_vm::JitCompileSnapshot;

mod abi;
mod artifacts;
mod code;
mod lowering;
mod runtime_ops;
mod value_abi;
pub(crate) use abi::*;
use artifacts::*;
pub use code::BaselineCode;
pub(crate) use code::enter_compiled;
pub use lowering::Unsupported;
use lowering::*;
pub(crate) use lowering::{BaselinePlan, MAX_METHOD_ARGS, pack_method_arg_regs, reg_offset};
use runtime_ops::*;
pub(crate) use runtime_ops::{IC_WAYS, WhiskerIcCell, jit_backedge_poll_stub};
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

#[cfg(target_arch = "aarch64")]
pub(crate) mod arm64;

/// Compile a function view to baseline arm64 code under the isolate-assigned
/// unique code-object identity, or report why not.
#[cfg(target_arch = "aarch64")]
pub fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<BaselineCode, Unsupported> {
    arm64::compile(view, code_object_id)
}

/// JIT-owned runtime transitions installed into the isolate entry table at
/// compiler-hook install. Each binding names its VM descriptor id and
/// signature family; the VM validates the pairing before installation and
/// rejects an installation that leaves any inventory slot vacant, so this
/// table is the complete machine inventory of baseline transition entries.
pub(crate) fn runtime_stub_bindings() -> Vec<otter_vm::JitRuntimeStubBinding> {
    use otter_vm::native_abi as abi;
    let binding = |descriptor: abi::RuntimeStubDescriptor,
                   entry_addr: usize|
     -> otter_vm::JitRuntimeStubBinding {
        otter_vm::JitRuntimeStubBinding {
            id: descriptor.id,
            signature: descriptor.signature,
            entry_addr,
        }
    };
    vec![
        binding(
            abi::STUB_JIT_BACKEDGE_POLL,
            jit_backedge_poll_stub as *const () as usize,
        ),
        binding(abi::STUB_JIT_ADD, jit_add_stub as *const () as usize),
        binding(abi::STUB_JIT_NEG, jit_neg_stub as *const () as usize),
        binding(
            abi::STUB_JIT_MATH_CALL,
            jit_math_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_GLOBAL,
            jit_load_global_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_ELEMENT,
            jit_load_element_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_ELEMENT,
            jit_store_element_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DEFINE_OWN_PROPERTY,
            jit_define_own_property_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_COLLECTION_METHOD_IC,
            jit_call_collection_method_ic_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FINISH_DIRECT_CALL_BAILED,
            jit_finish_direct_call_bailed_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_SELF_CALL_BAIL,
            jit_self_call_bail_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_PROP_WINDOW,
            jit_load_prop_window_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_PROP_WINDOW,
            jit_store_prop_window_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_DEFINE_DATA_PROPERTY,
            jit_define_data_property_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_STRING,
            jit_load_string_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_BUILTIN_ERROR,
            jit_load_builtin_error_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_MAKE_FN,
            jit_make_fn_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_MAKE_CLOSURE,
            jit_make_closure_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_NEW_OBJECT,
            jit_new_object_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_NEW_ARRAY,
            jit_new_array_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FRESH_UPVALUE,
            jit_fresh_upvalue_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_PREPARE_DIRECT_CALL,
            jit_prepare_direct_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_PREPARE_DIRECT_METHOD_CALL,
            jit_prepare_direct_method_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_PUSH_NATIVE_ACTIVATION,
            jit_push_native_activation_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_POP_NATIVE_ACTIVATION,
            jit_pop_native_activation_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_ABORT_DIRECT_CALL,
            jit_abort_direct_call_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_FINISH_DIRECT_CALL_RETURNED,
            jit_finish_direct_call_returned_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_LOAD_UPVALUE,
            jit_load_upvalue_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_UPVALUE,
            jit_store_upvalue_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_STORE_UPVALUE_CHECKED,
            jit_store_upvalue_checked_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_WRITE_BARRIER,
            jit_write_barrier_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_WRITE_BARRIER_WINDOW,
            jit_write_barrier_window_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_INLINE_CLOSURE_UPVALUES,
            jit_inline_closure_upvalues_stub as *const () as usize,
        ),
        binding(
            abi::STUB_JIT_MATH_RANDOM,
            otter_jit_math_random as *const () as usize,
        ),
    ]
}

/// Non-arm64 stub: the emitter is arm64-only for now.
#[cfg(not(target_arch = "aarch64"))]
pub fn compile(
    view: &JitCompileSnapshot,
    code_object_id: u64,
) -> Result<BaselineCode, Unsupported> {
    let _ = (view, code_object_id);
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
        BaselineCode, BaselinePlan, CallOperandView, JitCtx, JitEntry, JitRet, STATUS_RETURNED,
        Unsupported, VALUE_FALSE, VALUE_NULL, VALUE_TRUE, VALUE_UNDEFINED, const_index, reg,
        value_tag,
    };
    use otter_bytecode::{Op, Operand};
    use otter_vm::{JitCompileSnapshot, JitFunctionCode, jit::JitTestInstruction};

    const STRIDE: u32 = 4;

    fn compile(view: &JitCompileSnapshot) -> Result<BaselineCode, Unsupported> {
        super::compile(view, 1)
    }

    enum Exit {
        Returned(u64),
        Bailed(u32),
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
        view
    }

    #[test]
    fn transition_bindings_cover_the_descriptor_inventory() {
        use otter_vm::native_abi::{RUNTIME_STUB_DESCRIPTORS, RuntimeStubSignature};
        let bindings = super::runtime_stub_bindings();
        let mut seen = std::collections::BTreeSet::new();
        for binding in &bindings {
            assert!(seen.insert(binding.id), "duplicate binding {}", binding.id);
            let descriptor = RUNTIME_STUB_DESCRIPTORS[binding.id as usize - 1];
            assert_eq!(descriptor.id, binding.id);
            assert_eq!(descriptor.signature, binding.signature);
            assert_ne!(binding.entry_addr, 0);
        }
        // Exactly the JIT-owned slots (everything the VM phase leaves vacant).
        let jit_owned = RUNTIME_STUB_DESCRIPTORS
            .iter()
            .filter(|descriptor| {
                !matches!(
                    descriptor.signature,
                    RuntimeStubSignature::LeafValue2 | RuntimeStubSignature::AllocValue3
                )
            })
            .count();
        assert_eq!(bindings.len(), jit_owned);
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
            Exit::Bailed(pc) => panic!("nullish loose equality bailed at {pc}"),
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
            Exit::Bailed(pc) => panic!("numeric loose inequality bailed at {pc}"),
        }
    }

    // CodeBlock branch encoding: target instruction = current + 1 + rel.
    fn rel(from: usize, to: usize) -> i32 {
        to as i32 - from as i32 - 1
    }

    fn run(view: &JitCompileSnapshot, regs: &mut [u64]) -> Exit {
        exec(view, regs, 0, 0, 1 << 30)
    }

    /// Drive one compiled entry with explicit boundary probes: the SELF
    /// closure bits (self-recursive call guard), the interrupt byte, and the
    /// back-edge fuel budget. A local register-stack probe backs inline
    /// self-recursive callee windows.
    fn exec(
        view: &JitCompileSnapshot,
        regs: &mut [u64],
        self_closure: u64,
        interrupt: u8,
        fuel: u64,
    ) -> Exit {
        let code = compile(view).expect("compiles");
        let mut error = None;
        // Probe cells published through the test `VmThread`. The null-`vm`
        // poll stub treats a taken slow path as "continue", so an exhausted
        // fuel budget or a set interrupt byte exercises the boundary without
        // a live interpreter.
        let interrupt_probe: u8 = interrupt;
        let mut backedge_fuel_probe: u64 = fuel;
        let mut sync_reentry_depth_probe: u32 = 0;
        let mut reg_stack_probe = [0u64; 512];
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
            self_closure,
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
        // SAFETY: integer-only function; never dereferences the null vm/stack.
        let entry: JitEntry = unsafe { std::mem::transmute(code.entry_ptr_for_test()) };
        let JitRet { value, status } = entry(&mut ctx);
        if status == STATUS_RETURNED {
            Exit::Returned(value)
        } else {
            Exit::Bailed(native_frame.header.pc)
        }
    }

    fn expect_int(view: &JitCompileSnapshot, regs: &mut [u64], expected: i32) {
        match run(view, regs) {
            Exit::Returned(bits) => assert_eq!(unbox_i32(bits), expected),
            Exit::Bailed(pc) => panic!("expected Returned({expected}), got Bailed({pc})"),
        }
    }

    fn expect_f64(view: &JitCompileSnapshot, regs: &mut [u64], expected: f64) {
        match run(view, regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), expected),
            Exit::Bailed(pc) => panic!("expected Returned({expected}), got Bailed({pc})"),
        }
    }

    #[test]
    fn side_exit_publishes_exact_instruction_pc_after_committed_effects() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(2), Operand::Imm32(41)],
            ),
            (
                Op::Sub,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);
        let mut regs = [box_i32(10), VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0];

        assert!(matches!(run(&v, &mut regs), Exit::Bailed(1)));
        assert_eq!(regs[2], box_i32(41), "earlier instruction stays committed");
        assert_eq!(regs[3], 0, "side-exit instruction remains uncommitted");
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
        assert!(matches!(run(&v, &mut regs), Exit::Bailed(_)));
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
        assert!(matches!(run(&v, &mut regs), Exit::Bailed(_)));
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
                Exit::Bailed(pc) => panic!("cmp bailed at {pc}"),
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
                Exit::Bailed(_) => None,
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
        assert!(matches!(run(&v, &mut regs), Exit::Bailed(_)));
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
        assert!(matches!(run(&v, &mut regs), Exit::Bailed(_)));
    }

    #[test]
    fn adds_two_doubles() {
        let v = add_view();
        let mut regs = [box_f64(1.5), box_f64(2.25), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.75),
            Exit::Bailed(_) => panic!("expected 3.75, bailed"),
        }
    }

    #[test]
    fn mixes_int_and_double() {
        // int32(10) + double(2.5) → double(12.5): the int operand sign-converts.
        let v = add_view();
        let mut regs = [box_i32(10), box_f64(2.5), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 12.5),
            Exit::Bailed(_) => panic!("expected 12.5, bailed"),
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
            Exit::Bailed(_) => panic!("expected 3.5, bailed"),
        }
        // 6 / 2 yields the Number 3 (an f64), not an int32.
        let mut regs = [box_i32(6), box_i32(2), 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), 3.0),
            Exit::Bailed(_) => panic!("expected 3.0, bailed"),
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
            Exit::Bailed(_) => panic!("expected 2.5, bailed"),
        }
        // A non-number (undefined) still bails.
        let mut regs = [VALUE_UNDEFINED, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(run(&v, &mut regs), Exit::Bailed(_)));
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
            Exit::Bailed(_) => panic!("expected 3.5, bailed"),
        }
        // i32::MAX + 1 overflows → exact double.
        let mut regs = [box_i32(i32::MAX), 0, 0, 0, 0, 0, 0, 0];
        match run(&v, &mut regs) {
            Exit::Returned(bits) => assert_eq!(unbox_f64(bits), i32::MAX as f64 + 1.0),
            Exit::Bailed(_) => panic!("expected overflow→double, bailed"),
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
            Exit::Bailed(_) => panic!("expected 1e10, bailed"),
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
        assert_eq!(
            plan.instructions[0]
                .branch_operands()
                .map(|operands| operands.target),
            Ok(1)
        );
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
    fn lowering_plan_publishes_typed_fixed_operands() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(42)],
            ),
            (
                Op::LoadString,
                vec![Operand::Register(1), Operand::ConstIndex(0)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::Neg, vec![Operand::Register(3), Operand::Register(2)]),
            (
                Op::StoreLocal,
                vec![Operand::Register(3), Operand::Imm32(7)],
            ),
            (
                Op::LoadElement,
                vec![
                    Operand::Register(4),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(3),
                    Operand::Register(5),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        assert_eq!(plan.instructions.len(), v.instructions.len());
        assert_eq!(plan.instructions[0].op, Op::LoadInt32);
        assert_eq!(plan.instructions[0].byte_pc, v.instructions[0].byte_pc);
        let load = plan.instructions[0]
            .load_int32_operands()
            .expect("LoadInt32 operands");
        assert_eq!((load.dst, load.value), (0, 42));
        let string = plan.instructions[1]
            .constant_operands()
            .expect("LoadString operands");
        assert_eq!((string.dst, string.constant), (1, 0));
        let add = plan.instructions[2]
            .binary_operands()
            .expect("Add operands");
        assert_eq!((add.dst, add.lhs, add.rhs), (2, 0, 1));
        let neg = plan.instructions[3].unary_operands().expect("Neg operands");
        assert_eq!((neg.dst, neg.src), (3, 2));
        let store = plan.instructions[4]
            .local_operands()
            .expect("StoreLocal operands");
        assert_eq!((store.value, store.local), (3, 7));
        let load_element = plan.instructions[5]
            .element_load_operands()
            .expect("LoadElement operands");
        assert_eq!(
            (load_element.dst, load_element.receiver, load_element.index),
            (4, 0, 1)
        );
        let store_element = plan.instructions[6]
            .element_store_operands()
            .expect("StoreElement operands");
        assert_eq!(
            (
                store_element.receiver,
                store_element.index,
                store_element.value,
                store_element.scratch
            ),
            (0, 1, 3, 5)
        );
        assert_eq!(
            plan.instructions[7]
                .source_operands()
                .map(|operands| operands.src),
            Ok(3)
        );
    }

    #[test]
    fn lowering_plan_publishes_property_and_upvalue_operands() {
        let v = view(&[
            (
                Op::LoadProperty,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(7),
                ],
            ),
            (
                Op::StoreProperty,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(7),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (
                Op::LoadUpvalue,
                vec![Operand::Register(4), Operand::Imm32(5)],
            ),
            (
                Op::StoreUpvalueChecked,
                vec![Operand::Register(4), Operand::Imm32(5)],
            ),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        let load = plan.instructions[0]
            .property_load_operands()
            .expect("LoadProperty operands");
        assert_eq!((load.dst, load.object, load.name), (2, 0, 7));
        let store = plan.instructions[1]
            .property_store_operands()
            .expect("StoreProperty operands");
        assert_eq!(
            (store.object, store.name, store.value, store.scratch),
            (0, 7, 2, 3)
        );
        let load_upvalue = plan.instructions[2]
            .upvalue_operands()
            .expect("LoadUpvalue operands");
        assert_eq!((load_upvalue.value, load_upvalue.index), (4, 5));
        let store_upvalue = plan.instructions[3]
            .upvalue_operands()
            .expect("StoreUpvalueChecked operands");
        assert_eq!((store_upvalue.value, store_upvalue.index), (4, 5));
    }

    #[test]
    fn lowering_plan_owns_variadic_operand_tails() {
        let v = view(&[
            (
                Op::NewArray,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(2),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::MathCall,
                vec![
                    Operand::Register(3),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(2),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::MakeClosure,
                vec![
                    Operand::Register(4),
                    Operand::ConstIndex(9),
                    Operand::ConstIndex(2),
                    Operand::Imm32(0),
                    Operand::Imm32(1),
                ],
            ),
            (Op::FreshUpvalue, vec![Operand::Imm32(6)]),
            (
                Op::DefineDataProperty,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        let array = plan.instructions[0]
            .new_array_operands()
            .expect("NewArray operands");
        assert_eq!(array.dst, 0);
        assert_eq!(plan.register_tail(array.elements), Ok(&[1, 2][..]));
        let math = plan.instructions[1]
            .math_call_operands()
            .expect("MathCall operands");
        assert_eq!((math.dst, math.method), (3, 0));
        assert_eq!(plan.register_tail(math.arguments), Ok(&[1, 2][..]));
        let closure = plan.instructions[2]
            .make_closure_operands()
            .expect("MakeClosure operands");
        assert_eq!((closure.dst, closure.function), (4, 9));
        assert_eq!(plan.index_tail(closure.parents), Ok(&[0, 1][..]));
        assert_eq!(
            plan.instructions[3]
                .immediate_operands()
                .map(|operands| operands.value),
            Ok(6)
        );
        let triple = plan.instructions[4]
            .triple_operands()
            .expect("DefineDataProperty operands");
        assert_eq!((triple.first, triple.second, triple.third), (0, 1, 2));
    }

    #[test]
    fn lowering_plan_owns_call_operand_tails() {
        let v = view(&[
            (
                Op::Call,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(2),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::ConstIndex(7),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = BaselinePlan::build(&v).expect("plan");

        let call = plan.instructions[0].call_operands().expect("Call operands");
        assert_eq!((call.dst, call.callee), (0, 1));
        let call_view = CallOperandView::Plain {
            call,
            arguments: plan.register_tail(call.arguments).expect("Call arguments"),
        };
        assert_eq!(const_index(call_view, 2), Ok(2));
        assert_eq!((reg(call_view, 3), reg(call_view, 4)), (Ok(2), Ok(3)));

        let method = plan.instructions[1]
            .method_call_operands()
            .expect("CallMethodValue operands");
        assert_eq!((method.dst, method.receiver, method.name), (4, 1, 7));
        let method_view = CallOperandView::Method {
            call: method,
            arguments: plan
                .register_tail(method.arguments)
                .expect("method arguments"),
        };
        assert_eq!(const_index(method_view, 3), Ok(1));
        assert_eq!(reg(method_view, 4), Ok(2));
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

    // Opcode-boundary contract (JIT_REFACTOR_PLAN.md Phase 4 audit matrix).
    // The uncommitted-guard-exit row is covered by
    // `side_exit_publishes_exact_instruction_pc_after_committed_effects`;
    // below: interrupt polls carry zero partial effects, and nested
    // self-recursive calls bail at the call boundary on a guard miss and
    // return through nested frames when the guard holds.

    #[test]
    fn interrupt_and_fuel_polls_carry_no_partial_effects() {
        // Triangular-sum loop; every back-edge takes the slow poll path
        // (exhausted fuel, then a set interrupt byte). The null-`vm` poll
        // treats both as "continue", so the loop must still complete with the
        // exact same result: the poll boundary observes no partial state.
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
        for (interrupt, fuel) in [(0u8, 1u64), (1u8, 1 << 30)] {
            let mut regs = [box_i32(100), 0, 0, 0, 0, 0, 0, 0];
            match exec(&v, &mut regs, 0, interrupt, fuel) {
                Exit::Returned(bits) => assert_eq!(unbox_i32(bits), 5050),
                Exit::Bailed(pc) => panic!("poll boundary bailed at {pc}"),
            }
        }
    }

    /// Countdown-sum body eligible for the inline self-recursive call path:
    /// `f(n) = n <= 0 ? 0 : f(n - 1) + n`, with the SELF binding materialized
    /// through a `make_self` `MakeFunction`.
    fn self_recursive_sum_view() -> JitCompileSnapshot {
        let mut v = view(&[
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (
                Op::LessEq,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(rel(2, 4)), Operand::Register(2)],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
            (
                Op::MakeFunction,
                vec![Operand::Register(6), Operand::ConstIndex(0)],
            ),
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)]),
            (
                Op::Sub,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(4),
                ],
            ),
            (
                Op::Call,
                vec![
                    Operand::Register(5),
                    Operand::Register(6),
                    Operand::ConstIndex(1),
                    Operand::Register(3),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(7),
                    Operand::Register(5),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(7)]),
        ]);
        v.instructions[4].make_self = true;
        v
    }

    #[test]
    fn self_call_guard_miss_bails_at_the_call_boundary() {
        // The callee register holds undefined, never the entry's self-closure
        // sentinel, so the self-call identity guard misses and the exit lands
        // AT the call with every earlier effect committed.
        let miss = view(&[
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (
                Op::Call,
                vec![
                    Operand::Register(5),
                    Operand::Register(6),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ]);
        let sentinel_self = box_i32(777);
        let mut regs = [0, 0, 0, 0, 0, 0, VALUE_UNDEFINED, 0];
        match exec(&miss, &mut regs, sentinel_self, 0, 1 << 30) {
            Exit::Bailed(pc) => assert_eq!(pc, 1, "guard miss exits at the call"),
            Exit::Returned(bits) => panic!("expected call-boundary bail, returned {bits:#x}"),
        }
        assert_eq!(regs[1], box_i32(0), "committed effect before the call");
    }

    #[test]
    fn self_recursive_call_returns_through_nested_frames() {
        let v = self_recursive_sum_view();
        // The make_self op loads the entry's self-closure bits into the
        // callee register, so the call guard holds and recursion runs inline
        // through nested windows: f(10) = 55.
        let sentinel_self = box_i32(777);
        let mut regs = [box_i32(10), 0, 0, 0, 0, 0, 0, 0];
        match exec(&v, &mut regs, sentinel_self, 0, 1 << 30) {
            Exit::Returned(bits) => assert_eq!(unbox_i32(bits), 55),
            Exit::Bailed(pc) => panic!("self-recursive sum bailed at {pc}"),
        }
    }
}
