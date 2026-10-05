//! Actual Graph OSR entry and preservation of prior interpreter root ownership.
//!
//! # Contents
//! - A loop entered directly from a live interpreter window on each native target.
//! - Exact resume geometry, emitted OSR region and root restoration assertions.
//!
//! # Invariants
//! The prefix computes a different result, so ordinary entry cannot satisfy the
//! proof. The installed executable and its complete recovery sidecars remain
//! alive for the call; every input is initialized and the stack is real.
//!
//! # See also
//! - `crate::graph::metadata` owns the initialized tagged root region.

use super::*;

#[test]
fn native_osr_consumes_live_loop_values_and_restores_the_prior_interpreter_roots() {
    let mut view = snapshot(
        2,
        3,
        vec![
            (
                Op::LoadInt32,
                vec![Operand::Register(2), Operand::Imm32(777)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Register(0),
                ],
            ),
            (
                Op::SubImm,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Imm32(1),
                ],
            ),
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(-3), Operand::Register(1)],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    seed(&mut view, &[1, 2], ARITH_INT32);
    let compiled =
        crate::graph::compile(&view, 8011, &TransitionTable::resolve(), Some(1), true).unwrap();
    assert!(compiled.built.osr_entry.is_some());
    let end = compiled
        .emission
        .osr_dispatch_end
        .expect("native OSR dispatch");
    assert!(end > compiled.emission.tier_entry);
    let entry: JitEntry = unsafe {
        std::mem::transmute(
            compiled
                .emission
                .buffer
                .ptr(dynasmrt::AssemblyOffset(compiled.emission.tier_entry)),
        )
    };
    let mut window = [int(3), int(4), int(10)];
    let mut prior_roots = [int(-199)];
    let mut frame = Frame::new(
        VmFrameHeader {
            function_id: view.code_block.id,
            pc: 1,
            register_count: 3,
            kind: NativeFrameKind::Optimizing,
            flags: NativeFrameFlags::from_bits(NativeFrameFlags::OSR_ENTRY),
        },
        window.as_mut_ptr() as u64,
        Value::undefined(),
        Value::undefined(),
    );
    frame.machine_roots = prior_roots.as_mut_ptr() as u64;
    frame.call_site = 11;
    let prior_depth = frame.depth;
    let prior_call_site = frame.call_site;
    let prior_root_base = frame.machine_roots;
    let interrupt = 0u8;
    let mut fuel = i64::MAX as u64;
    let mut thread = VmThread::empty();
    thread.interrupt_cell = std::ptr::addr_of!(interrupt) as u64;
    thread.backedge_fuel_cell = std::ptr::addr_of_mut!(fuel) as u64;
    let mut error = None;
    let mut ctx = JitCtx {
        thread: &mut thread,
        native_frame: &mut frame,
        error: &mut error,
        generated_depth_limit: u64::MAX,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        completion_destination: u32::MAX,
        completion_generation: 0,
        pending_call: otter_vm::native_abi::CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
    };
    thread.frame_cell = std::ptr::addr_of_mut!(ctx.native_frame) as u64;
    assert_eq!(
        thread.frame_cell,
        std::ptr::addr_of_mut!(ctx.native_frame) as u64
    );
    let result = entry(&mut ctx);
    assert_eq!(
        returned(result),
        int(22),
        "OSR must skip the 777 prefix and use the live phi inputs"
    );
    assert_eq!(frame.header.flags.bits() & NativeFrameFlags::OSR_ENTRY, 0);
    assert_eq!(frame.machine_roots, prior_root_base);
    assert_eq!(frame.depth, prior_depth);
    assert_eq!(frame.call_site, prior_call_site);
    assert_eq!(prior_roots, [int(-199)]);
    assert_eq!(ctx.native_frame, std::ptr::addr_of_mut!(frame));
}
