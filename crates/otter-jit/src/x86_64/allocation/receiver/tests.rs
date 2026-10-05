//! Executable private-memory proofs of the production receiver encoder.
//!
//! # Contents
//! - Closure/class live prototype proofs and exact initialized shell bytes.
//! - Exact-family, unfinished-root, validity, shape and LAB misses before publication.
//! - Exact allocation accounting and immutable family payload and preserved call operands/registers.
//!
//! # Invariants
//! - Fabricated host cells never enter the collector; these tests claim no GC.
//! - The private System V wrapper saves the complete nonvolatile register bank.
//! - The completed candidate retains the original LAB top until publication;
//!   the fixture then checks every payload byte and its surrounding guards.
//! - Runtime fixtures separately prove actual generated callees and movement.

use super::*;
use crate::CompiledCode;
use otter_gc::{lab::LinearAllocationArea, stats::TypeStats};
use otter_vm::{
    JitRuntimeStats, Value,
    jit::{
        JitClassConstructorLayout, JitClosureCallLayout, JitConstructorLayout, JitPrototypeValidity,
    },
    native_abi::{CallRequest, JitCtx, NativeResultPair, VmThread},
};

const POISON: u64 = 0xcafe_1234_5678_abcd;
const FUNCTION: u32 = 0xf123_4567;
const CANARY: u64 = 0x1357_2468_ace0_bdf1;

#[derive(Clone, Copy, Debug)]
enum Case {
    Closure,
    EmptyShape,
    ClassFunction,
    ClassClosure,
    ClassWrongFunction,
    ClassNullPrototype,
    GuardClass,
    NoRare,
    NoFamily,
    DifferentFamily,
    UnfinishedFamily,
    WrongFunction,
    ChangedFamilyRoot,
    NullPrototype,
    PrimitivePrototype,
    RootPrototype,
    InitialPrototype,
    InvalidValidity,
    Space,
}

fn store32(words: &mut [u64], byte: usize, value: u32) {
    let old = words[byte / 8];
    let shift = byte % 8 * 8;
    words[byte / 8] = (old & !(u64::from(u32::MAX) << shift)) | (u64::from(value) << shift);
}

fn execute(case: Case) {
    let mut memory = vec![0_u64; 512];
    let base = memory.as_mut_ptr() as usize;
    let cage = base & !(u32::MAX as usize);
    let offset = |byte: usize| u32::try_from(base + byte - cage).unwrap();
    let root = 128;
    let initial = 192;
    let prototype = 256;
    let family = 320;
    let rare = 384;
    let closure = 448;
    let class = 576;
    let start_byte = 1024;
    let start = base + start_byte;
    let mut validity = u32::from(!matches!(case, Case::InvalidValidity));
    let mut view = JitCompileSnapshot::without_feedback(7, 0, 1, vec![]);
    view.cage_base = cage;
    view.object_shape_byte = 8;
    view.object_exotic_handle_byte = 16;
    view.shape_property_count_byte = 8;
    view.shape_prototype_byte = 12;
    view.closure_call_layout = JitClosureCallLayout {
        function_id_byte: 8,
        rare_byte: 12,
        constructor_layouts_byte: 8,
        prototype_byte: 16,
        ..JitClosureCallLayout::default()
    };
    view.class_constructor_layout = JitClassConstructorLayout {
        type_tag: 0x44,
        callable_byte: 8,
        super_constructor_byte: 24,
        prototype_byte: 16,
        constructor_layouts_byte: 32,
    };
    view.constructor_layout = JitConstructorLayout {
        family_id_byte: 8,
        root_byte: 16,
        samples_remaining_byte: 20,
    };
    let plan = JitReceiverAllocationPlan {
        new_target_function_id: FUNCTION,
        new_target_is_class: matches!(
            case,
            Case::ClassFunction
                | Case::ClassClosure
                | Case::ClassWrongFunction
                | Case::ClassNullPrototype
        ),
        family_id: 0xfedc_ba98_7654_3210,
        receiver_shape: if matches!(case, Case::EmptyShape) {
            0
        } else {
            offset(initial)
        },
        initial_field_count: if matches!(case, Case::EmptyShape) {
            0
        } else {
            2
        },
        inline_capacity: 4,
        prototype_validity: Some(JitPrototypeValidity {
            address: std::ptr::addr_of_mut!(validity) as usize,
            identity: 7,
        }),
        prototype_root: offset(root),
    };
    store32(
        &mut memory,
        root + 12,
        if matches!(case, Case::RootPrototype) {
            offset(prototype) + 8
        } else {
            offset(prototype)
        },
    );
    store32(
        &mut memory,
        initial + 12,
        if matches!(case, Case::InitialPrototype) {
            offset(prototype) + 8
        } else {
            offset(prototype)
        },
    );
    store32(&mut memory, initial + 8, 2);
    memory[prototype / 8] = OBJECT_BODY_TYPE_TAG as u64;
    memory[(family + 8) / 8] = plan.family_id - u64::from(matches!(case, Case::DifferentFamily));
    store32(
        &mut memory,
        family + 16,
        if matches!(case, Case::ChangedFamilyRoot) {
            offset(initial)
        } else {
            offset(root)
        },
    );
    store32(
        &mut memory,
        family + 20,
        u32::from(matches!(case, Case::UnfinishedFamily)),
    );
    let family_before = memory[family / 8..(family + 32) / 8].to_vec();
    store32(
        &mut memory,
        rare + 8,
        if matches!(case, Case::NoFamily) {
            0
        } else {
            offset(family)
        },
    );
    store32(
        &mut memory,
        class + 32,
        if matches!(case, Case::NoFamily) {
            0
        } else {
            offset(family)
        },
    );
    memory[closure / 8] = JS_CLOSURE_BODY_TYPE_TAG as u64;
    store32(
        &mut memory,
        closure + 8,
        if matches!(case, Case::WrongFunction) {
            FUNCTION - 1
        } else {
            FUNCTION
        },
    );
    store32(
        &mut memory,
        closure + 12,
        if matches!(case, Case::NoRare) {
            0
        } else {
            offset(rare)
        },
    );
    memory[(rare + 16) / 8] = match case {
        Case::NullPrototype => 0,
        Case::PrimitivePrototype => Value::number_i32(1).to_bits(),
        _ => (base + prototype) as u64,
    };
    memory[class / 8] = u64::from(view.class_constructor_layout.type_tag);
    memory[(class + 8) / 8] = if matches!(case, Case::ClassClosure | Case::ClassWrongFunction) {
        (base + closure) as u64
    } else {
        otter_vm::value::tag::box_function_id(FUNCTION)
    };
    if matches!(case, Case::ClassWrongFunction) {
        store32(&mut memory, closure + 8, FUNCTION - 1);
    }
    store32(
        &mut memory,
        class + 16,
        if matches!(case, Case::ClassNullPrototype) {
            0
        } else {
            offset(prototype)
        },
    );
    let new_target = base
        + if matches!(
            case,
            Case::ClassFunction
                | Case::ClassClosure
                | Case::ClassWrongFunction
                | Case::ClassNullPrototype
                | Case::GuardClass
        ) {
            class
        } else {
            closure
        };
    let bytes = view
        .field_layout
        .cell_bytes(usize::from(plan.inline_capacity));
    memory[start_byte / 8 - 1..(start_byte + bytes) / 8 + 1].fill(POISON);
    let mut lab = LinearAllocationArea {
        top: start,
        limit: start + bytes - usize::from(matches!(case, Case::Space)),
    };
    let mut stats = [TypeStats::DEFAULT; 256];
    let mut runtime = JitRuntimeStats::default();
    let mut thread = VmThread::empty();
    let mut ctx = JitCtx {
        thread: &mut thread,
        native_frame: std::ptr::null_mut(),
        error: std::ptr::null_mut(),
        generated_depth_limit: u64::MAX,
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow {
            lab: &mut lab,
            type_stats: stats.as_mut_ptr(),
        },
        runtime_stats: &mut runtime,
        pending_call: CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
        completion_destination: u32::MAX,
        completion_generation: 0,
    };
    let mut output = [0_u64; 12];
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let missed = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; push rbx ; push rbp ; push r12 ; push r13 ; push r14 ; push r15
        ; mov r15, rdi ; mov rcx, rsi ; mov r14, rdx
        ; mov rsi, QWORD CANARY as i64
        ; mov rbx, QWORD CANARY as i64 ; mov r12, QWORD CANARY as i64
        ; mov r13, QWORD CANARY as i64 ; mov rbp, QWORD CANARY as i64
        ; mov r10, QWORD CANARY as i64 ; movq xmm0, r10 ; movq xmm15, r10
    );
    let mut relocations = RelocationCapture::default();
    emit_receiver_candidate_probe(&mut ops, &mut relocations, &view, plan, 15);
    dynasm!(ops ; .arch x64
        ; mov [r14], rax ; mov [r14 + 8], rdx
        ; test rdx, rdx ; jz =>missed
        // The fit is still unpublished. Read the original top through its LAB.
        ; mov r10, [rdx + crate::entry::LAB_TOP_OFFSET as i32]
        ; mov [r14 + 16], r10
    );
    emit_receiver_publication_effect(&mut ops, &view, 15);
    dynasm!(ops ; .arch x64
        ; =>missed
        ; mov [r14 + 24], rsi ; mov [r14 + 32], rcx
        ; movq r10, xmm0 ; mov [r14 + 40], r10
        ; movq r10, xmm15 ; mov [r14 + 48], r10
        ; mov [r14 + 56], rbx ; mov [r14 + 64], rbp
        ; mov [r14 + 72], r12 ; mov [r14 + 80], r13 ; mov [r14 + 88], rax
        ; jmp =>done ; =>done
        ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; pop rbp ; pop rbx ; ret
    );
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the private System V entry touches only these bounded host
    // cells/LAB/stats/output, saves nonvolatiles and never calls or enters GC.
    let call: extern "sysv64" fn(*mut JitCtx, usize, *mut u64) =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    call(&mut ctx, new_target, output.as_mut_ptr());
    let accepted = matches!(
        case,
        Case::Closure | Case::EmptyShape | Case::ClassFunction | Case::ClassClosure
    );
    assert_eq!(
        output[0],
        if accepted {
            start as u64
        } else {
            VALUE_UNDEFINED
        },
        "{case:?}"
    );
    assert_eq!(output[1] != 0, accepted, "{case:?}");
    assert_eq!(output[3], CANARY);
    assert_eq!(output[4], new_target as u64);
    assert_eq!(output[5], CANARY);
    assert_eq!(output[6], CANARY);
    assert!(output[7..11].iter().all(|word| *word == CANARY));
    assert_eq!(output[11], output[0], "publication preserves the candidate");
    assert_eq!(memory[start_byte / 8 - 1], POISON);
    assert_eq!(memory[(start_byte + bytes) / 8], POISON);
    if accepted {
        assert_eq!(
            output[2], start as u64,
            "candidate must remain unpublished until the effect"
        );
        assert_eq!(lab.top, start + bytes);
        let mut expected = vec![0_u64; bytes / 8];
        expected[0] = plan.cell_header_word(bytes as u32);
        store32(
            &mut expected,
            view.object_shape_byte as usize,
            if plan.receiver_shape == 0 {
                plan.prototype_root
            } else {
                plan.receiver_shape
            },
        );
        for index in 0..u32::from(plan.inline_capacity) {
            let byte = view
                .field_layout
                .inline_byte(otter_vm::object::FieldLocation::inline(index));
            expected[byte as usize / 8] = VALUE_UNDEFINED;
        }
        assert_eq!(
            &memory[start_byte / 8..(start_byte + bytes) / 8],
            expected,
            "{case:?}: complete initialized payload"
        );
    } else {
        assert_eq!(lab.top, start);
        assert!(
            memory[start_byte / 8..(start_byte + bytes) / 8]
                .iter()
                .all(|word| *word == POISON)
        );
    }
    assert_eq!(
        &memory[family / 8..(family + 32) / 8],
        family_before,
        "{case:?}: no probe/publication may mutate or retain a family receiver"
    );
    assert_eq!(runtime.receiver_alloc_attempts, 1);
    assert_eq!(runtime.receiver_alloc_generated, u64::from(accepted));
    assert_eq!(
        runtime.receiver_alloc_space_misses,
        u64::from(matches!(case, Case::Space))
    );
    assert_eq!(
        runtime.receiver_alloc_guard_misses,
        u64::from(!accepted && !matches!(case, Case::Space))
    );
    for (tag, row) in stats.iter().enumerate() {
        let owned = accepted && tag == OBJECT_BODY_TYPE_TAG as usize;
        assert_eq!(row.live_bytes, if owned { bytes } else { 0 });
        assert_eq!(row.alloc_count_total, u64::from(owned));
        assert_eq!(row.alloc_bytes_total, if owned { bytes as u64 } else { 0 });
        assert_eq!(row.free_count_total, 0);
    }
}

#[test]
fn native_receiver_live_proofs_initialize_then_publish_once() {
    for case in [
        Case::Closure,
        Case::EmptyShape,
        Case::ClassFunction,
        Case::ClassClosure,
    ] {
        execute(case);
    }
}

#[test]
fn native_receiver_pre_effect_misses_preserve_call_operands_and_poison() {
    for case in [
        Case::GuardClass,
        Case::ClassWrongFunction,
        Case::ClassNullPrototype,
        Case::NoRare,
        Case::NoFamily,
        Case::DifferentFamily,
        Case::UnfinishedFamily,
        Case::WrongFunction,
        Case::ChangedFamilyRoot,
        Case::NullPrototype,
        Case::PrimitivePrototype,
        Case::RootPrototype,
        Case::InitialPrototype,
        Case::InvalidValidity,
        Case::Space,
    ] {
        execute(case);
    }
}
