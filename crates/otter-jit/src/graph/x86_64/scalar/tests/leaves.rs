//! Executable Graph predicate/leaf fixtures with every eligible live register.
//!
//! # Contents
//! - Actual scalar node dispatch over explicit legal allocated locations.
//! - Pure float remainder/ToInt32 and fast/cold tagged predicate results.
//! - Complete GP/FP preservation around typed VM C boundaries.
//!
//! # Invariants
//! - Test wrappers preserve their own C nonvolatile registers and align calls.
//! - The real heap/context remain live; no fixture simulates collection.
//! - Only a node's result and declared FP temporary may replace canaries.

use super::*;
use crate::graph::registers::X86_64;
use otter_vm::jit::JitTestInstruction;
use otter_vm::native_abi::{JitCtx, NativeResultPair, VmThread};

fn gp_canary(register: u8) -> u64 {
    0x7183_abcd_1738_0000 | u64::from(register)
}
fn fp_canary(register: u8) -> u64 {
    0x8000_7319_abcd_0000 | u64::from(register)
}

/// Emit only the real scalar node and its cold leaves. Other Codegen fields
/// are inert; no entry, frame, generic transition or runtime dispatcher is used.
fn fixture(kind: Kind) -> crate::CompiledCode {
    let view = JitCompileSnapshot::without_feedback(
        90,
        2,
        2,
        vec![JitTestInstruction::new(
            otter_bytecode::Op::ReturnUndefined,
            0,
            0,
            vec![],
        )],
    );
    let transitions = crate::entry::TransitionTable::resolve();
    let plan = crate::template::TemplatePlan::build_unfused(&view).unwrap();
    let mut graph = Graph::default();
    let floating = matches!(kind, Kind::Float64Mod | Kind::TruncateFloat64ToInt32);
    let binary = matches!(kind, Kind::Float64Mod | Kind::StrictEqual { .. });
    let a = graph.add_node(
        Kind::InitialRegister(0),
        &[],
        if floating {
            Repr::Float64
        } else {
            Repr::Tagged
        },
    );
    let b = graph.add_node(
        Kind::InitialRegister(1),
        &[],
        if floating {
            Repr::Float64
        } else {
            Repr::Tagged
        },
    );
    let inputs = if binary { vec![a, b] } else { vec![a] };
    let node = graph.add_node(
        kind.clone(),
        &inputs,
        if kind == Kind::Float64Mod {
            Repr::Float64
        } else if kind == Kind::TruncateFloat64ToInt32 {
            Repr::Int32
        } else {
            Repr::Tagged
        },
    );
    let result = if kind == Kind::Float64Mod {
        Location::Fp(2)
    } else {
        Location::Gp(0)
    };
    let mut allocation = Allocation::default();
    allocation
        .nodes
        .resize(graph.nodes.len(), Default::default());
    allocation.nodes[node.0 as usize].inputs.push(if floating {
        Location::Fp(0)
    } else {
        Location::Gp(7)
    });
    if binary {
        allocation.nodes[node.0 as usize].inputs.push(if floating {
            Location::Fp(1)
        } else {
            Location::Gp(6)
        });
    }
    allocation.nodes[node.0 as usize].result = Some(result);
    if matches!(kind, Kind::StrictEqual { .. }) {
        allocation.nodes[node.0 as usize].fp_temps.push(14);
    }
    for &register in X86_64.general {
        if result != Location::Gp(register) {
            allocation.nodes[node.0 as usize]
                .live_registers
                .push((Location::Gp(register), Repr::Word));
        }
    }
    for register in 0..15 {
        if result != Location::Fp(register)
            && !(register == 14 && matches!(kind, Kind::StrictEqual { .. }))
        {
            allocation.nodes[node.0 as usize]
                .live_registers
                .push((Location::Fp(register), Repr::Float64));
        }
    }
    let slots = SlotLayout::of(&allocation).unwrap();
    let mut ops = Assembler::new().unwrap();
    let start = ops.offset();
    let returned = ops.new_dynamic_label();
    let deopt = ops.new_dynamic_label();
    let fatal = ops.new_dynamic_label();
    let construct = ops.new_dynamic_label();
    let side_exit = ops.new_dynamic_label();
    let threw = ops.new_dynamic_label();
    let committed_throw = ops.new_dynamic_label();
    let propagate = ops.new_dynamic_label();
    let materialize = ops.new_dynamic_label();
    let mut codegen = Codegen {
        ops,
        relocations: RelocationCapture::new(false),
        view: &view,
        inline_views: &[],
        graph: &graph,
        allocation: &allocation,
        layout: &[],
        slots,
        sp_delta: 0,
        labels: FxHashMap::default(),
        exits: Vec::new(),
        deferred: Vec::new(),
        returned,
        deopt,
        fatal,
        activation: crate::frame::ActivationExits {
            construct,
            side_exit,
        },
        spill: crate::frame::SpillArea {
            bytes: slots.bytes(),
            scratch_slot: Some(slots.spill_tagged),
            safepoint: FIRST_SITE_SAFEPOINT,
        saved_pairs: 0,
        },
        transitions: &transitions,
        deopt_runtime: 0,
        plan: &plan,
        plan_index: FxHashMap::default(),
        load_ic_cells: Box::new([]),
        next_load_ic: 0,
        store_ic_cells: Box::new([]),
        next_store_ic: 0,
        no_direct_call_events: None,
        no_code_map: None,
        spliced_functions: Default::default(),
        node_offsets: Vec::new(),
        threw,
        committed_throw,
        propagate,
        materialize,
        sites: crate::graph::metadata::SitePlan::new(slots.spill_tagged),
        return_sites: Vec::new(),
    };
    // Four saved nonvolatile words plus 24 local bytes give 16-byte alignment.
    dynasm!(codegen.ops ; .arch x64
        ; push rbp ; mov rbp, rsp ; push rbx ; push r12 ; push r15 ; sub rsp, 24
        ; mov [rbp - 32], rsi ; mov r15, rdi
        ; mov [rbp - 40], rdx ; mov [rbp - 48], rcx
    );
    for &register in X86_64.general {
        codegen.load_immediate(register, gp_canary(register));
    }
    // Floating C arguments already occupy XMM0/1. Tagged arguments were
    // parked above before the wrapper seeded the complete GP allocation bank.
    if !floating {
        dynasm!(codegen.ops ; .arch x64 ; mov rdi, [rbp - 40] ; mov rsi, [rbp - 48]);
    }
    for register in 0_u8..15 {
        if !floating || register > 1 {
            codegen.load_immediate(10, fp_canary(register));
            dynasm!(codegen.ops ; .arch x64 ; movq Rx(register), r10);
        }
    }
    assert!(codegen.emit_scalar(node).unwrap());
    assert_eq!(codegen.sp_delta, 0, "hot scalar preserves its stack delta");
    dynasm!(codegen.ops ; .arch x64 ; mov r11, [rbp - 32]);
    for &register in X86_64.general {
        let offset = i32::from(register) * 8;
        dynasm!(codegen.ops ; .arch x64 ; mov [r11 + offset], Rq(register));
    }
    for register in 0_u8..15 {
        let offset = 16 * 8 + i32::from(register) * 8;
        dynasm!(codegen.ops ; .arch x64 ; movsd [r11 + offset], Rx(register));
    }
    dynasm!(codegen.ops ; .arch x64 ; add rsp, 24 ; pop r15 ; pop r12 ; pop rbx ; pop rbp ; ret);
    while let Some(deferred) = codegen.deferred.pop() {
        deferred(&mut codegen);
        assert_eq!(
            codegen.sp_delta, 0,
            "cold leaf restores its complete save span"
        );
    }
    assert!(
        codegen.exits.is_empty(),
        "pure predicates and scalar leaves have no eager/committed exits"
    );
    crate::CompiledCode::new(codegen.ops.finalize().unwrap(), start)
}

fn context(heap: &otter_gc::GcHeap, thread: &mut VmThread) -> JitCtx {
    thread.gc_heap = std::ptr::from_ref(heap) as u64;
    JitCtx {
        thread,
        native_frame: std::ptr::null_mut(),
        error: std::ptr::null_mut(),
        generated_depth_limit: u64::MAX,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        completion_destination: u32::MAX,
        completion_generation: 0,
        pending_call: otter_vm::native_abi::CallRequest::EMPTY,
        completion: NativeResultPair::success(otter_vm::Value::UNDEFINED),
    }
}

fn assert_live(observed: &[u64; 31], kind: &Kind, a: u64, b: u64) {
    let floating = matches!(kind, Kind::Float64Mod | Kind::TruncateFloat64ToInt32);
    for &register in X86_64.general {
        if register == 0 && *kind != Kind::Float64Mod {
            continue;
        }
        let expected = if !floating && register == 7 {
            a
        } else if !floating && register == 6 {
            b
        } else {
            gp_canary(register)
        };
        assert_eq!(
            observed[usize::from(register)],
            expected,
            "{kind:?} preserves GP{register}"
        );
    }
    for register in 0_u8..15 {
        if (register == 2 && *kind == Kind::Float64Mod)
            || (register == 14 && matches!(kind, Kind::StrictEqual { .. }))
        {
            continue;
        }
        let expected = if floating && register == 0 {
            a
        } else if floating && register == 1 {
            b
        } else {
            fp_canary(register)
        };
        assert_eq!(
            observed[16 + usize::from(register)],
            expected,
            "{kind:?} preserves XMM{register}"
        );
    }
}

#[test]
fn typed_float_leaves_restore_every_live_gp_fp_before_committing_the_result() {
    let heap = otter_gc::GcHeap::new().unwrap();
    let mut thread = VmThread::empty();
    let mut ctx = context(&heap, &mut thread);
    for kind in [Kind::Float64Mod, Kind::TruncateFloat64ToInt32] {
        let code = fixture(kind.clone());
        // SAFETY: the wrapper follows System V, preserves nonvolatile registers,
        // and uses this live heap context only for typed NoAlloc VM leaves.
        let run: extern "sysv64" fn(*mut JitCtx, *mut u64, f64, f64) =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        for (a, b) in [
            (-0.0, 3.0),
            (-17.5, 4.0),
            (f64::INFINITY, 2.0),
            (f64::NAN, 7.0),
            (4_294_967_297.75, 13.5),
            (i32::MIN as f64, -1.0),
        ] {
            let mut observed = [0_u64; 31];
            run(&mut ctx, observed.as_mut_ptr(), a, b);
            assert_live(&observed, &kind, a.to_bits(), b.to_bits());
            if kind == Kind::Float64Mod {
                let expected = a % b;
                let actual = f64::from_bits(observed[18]);
                if expected.is_nan() {
                    assert!(actual.is_nan());
                } else {
                    assert_eq!(actual.to_bits(), expected.to_bits());
                }
            } else {
                let expected = if !a.is_finite() || a == 0.0 {
                    0
                } else {
                    let unsigned = a.trunc().rem_euclid(4_294_967_296.0) as u32;
                    unsigned as i32
                };
                assert_eq!(observed[0], u64::from(expected as u32));
            }
        }
    }
}

#[test]
fn tagged_predicates_keep_all_live_words_and_numeric_nan_zero_semantics() {
    let heap = otter_gc::GcHeap::new().unwrap();
    let mut thread = VmThread::empty();
    let mut ctx = context(&heap, &mut thread);
    let values = [
        tag::VALUE_FALSE,
        tag::VALUE_TRUE,
        tag::VALUE_NULL,
        tag::VALUE_UNDEFINED,
        tag::box_int32(0),
        tag::box_int32(-731),
        tag::box_double((-0.0_f64).to_bits()),
        tag::box_double(0.0_f64.to_bits()),
        tag::box_double(1.5_f64.to_bits()),
        tag::box_double(tag::CANONICAL_NAN),
    ];
    for kind in [
        Kind::StrictEqual { negate: false },
        Kind::StrictEqual { negate: true },
        Kind::ToBoolean,
        Kind::LogicalNot,
    ] {
        let code = fixture(kind.clone());
        // SAFETY: canonical tagged inputs, a live heap for cold predicates,
        // and an owned output window; the wrapper preserves C nonvolatiles.
        let run: extern "sysv64" fn(*mut JitCtx, *mut u64, u64, u64) =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        for a in values {
            for b in values {
                let mut observed = [0_u64; 31];
                run(&mut ctx, observed.as_mut_ptr(), a, b);
                assert_live(&observed, &kind, a, b);
                let number = |bits| {
                    if tag::is_int32_bits(bits) {
                        f64::from(bits as i32)
                    } else {
                        f64::from_bits(tag::unbox_double(bits))
                    }
                };
                let truthy = if tag::is_number_bits(a) {
                    let value = number(a);
                    value != 0.0 && !value.is_nan()
                } else {
                    a != tag::VALUE_FALSE && a != tag::VALUE_UNDEFINED && a != tag::VALUE_NULL
                };
                let equal = if tag::is_number_bits(a) && tag::is_number_bits(b) {
                    number(a) == number(b)
                } else {
                    a == b
                };
                let expected = match kind {
                    Kind::StrictEqual { negate } => equal ^ negate,
                    Kind::ToBoolean => truthy,
                    _ => !truthy,
                };
                assert_eq!(
                    observed[0],
                    if expected {
                        tag::VALUE_TRUE
                    } else {
                        tag::VALUE_FALSE
                    },
                    "{kind:?}: {a:#x}, {b:#x}"
                );
            }
        }
    }
}

#[test]
fn content_and_heap_truthiness_cold_leaves_preserve_the_complete_register_bank() {
    let codes: Vec<_> = [
        Kind::StrictEqual { negate: false },
        Kind::StrictEqual { negate: true },
        Kind::ToBoolean,
        Kind::LogicalNot,
    ]
    .into_iter()
    .map(|kind| (fixture(kind.clone()), kind))
    .collect();
    let mut interpreter = otter_vm::Interpreter::new().expect("scalar leaf realm bootstrap");
    otter_vm::NativeCtx::with_host_context(
        &mut interpreter,
        otter_vm::NativeCallInfo::default_call(),
        None,
        |native| -> Result<otter_vm::Value, otter_vm::NativeError> {
            // All JS allocations remain handle-rooted. After this one escaped
            // container is built, only declared NoAlloc leaves consume it.
            let holder = native.scope(|mut scope| {
                let holder = scope.array(0)?;
                let a = scope.string("scalar leaf 🙂")?;
                let b = scope.string("scalar leaf 🙂")?;
                let c = scope.string("scalar leaf 🙃")?;
                let empty = scope.string("")?;
                let big_a = scope.bigint_i128(0x1738_ffff_7319_9137)?;
                let big_b = scope.bigint_i128(0x1738_ffff_7319_9137)?;
                let big_zero = scope.bigint_i128(0)?;
                let object_a = scope.object()?;
                let object_b = scope.object()?;
                for (index, value) in [a, b, c, empty, big_a, big_b, big_zero, object_a, object_b]
                    .into_iter()
                    .enumerate()
                {
                    scope.set_index(holder, index, value)?;
                }
                Ok::<_, otter_vm::NativeError>(scope.finish(holder))
            })?;
            let array = holder.as_array().unwrap();
            let values: Vec<_> = (0..9)
                .map(|index| otter_vm::array::get(array, native.heap(), index).to_bits())
                .collect();
            assert_ne!(values[0], values[1], "equal strings have distinct cells");
            assert_ne!(values[4], values[5], "equal BigInts have distinct cells");
            let mut thread = VmThread::empty();
            let mut ctx = context(native.heap(), &mut thread);
            for (code, kind) in &codes {
                // SAFETY: all subject cells remain in this live heap and no JS
                // allocation occurs after extraction; each entry calls only
                // typed NoAlloc predicates and preserves C nonvolatiles.
                let run: extern "sysv64" fn(*mut JitCtx, *mut u64, u64, u64) =
                    unsafe { std::mem::transmute(code.entry_ptr()) };
                for (left, right, equal) in [
                    (0, 1, true),
                    (0, 2, false),
                    (4, 5, true),
                    (4, 6, false),
                    (7, 7, true),
                    (7, 8, false),
                    (0, 4, false),
                    (3, 6, false),
                ] {
                    let (a, b) = (values[left], values[right]);
                    let mut observed = [0_u64; 31];
                    run(&mut ctx, observed.as_mut_ptr(), a, b);
                    assert_live(&observed, kind, a, b);
                    let truthy = left != 3 && left != 6;
                    let expected = match kind {
                        Kind::StrictEqual { negate } => equal ^ negate,
                        Kind::ToBoolean => truthy,
                        _ => !truthy,
                    };
                    assert_eq!(
                        observed[0],
                        if expected {
                            tag::VALUE_TRUE
                        } else {
                            tag::VALUE_FALSE
                        },
                        "{kind:?}: cells {left}, {right}"
                    );
                }
            }
            Ok(otter_vm::Value::UNDEFINED)
        },
    )
    .unwrap();
}
