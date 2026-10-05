//! Executable shared-probe geometry with no collector-owned test pointers.
//!
//! # Contents
//! - One exact key carries inherited reads and receiver append actions.
//! - Full input/canary preservation and untouched pre-effect refusals.
//! - Live inline/slab bounds and independent own writable authorization.
//!
//! # Invariants
//! The aligned host arena models the scalar native DTO; it never enters GC.
//! FieldLayout and source metadata come from the existing VM snapshot. The
//! model explicitly assigns nonoverlapping object/shape offsets; its shape-ID
//! offset is fixture-only. VM/runtime tests own actual
//! table publication, descriptor semantics, weak clearing and moving roots.

use super::*;
use crate::CompiledCode;
use otter_bytecode::{BytecodeModule, Function, FunctionCodeBuilder, Op, SourceKind};
use otter_vm::{ExecutionContext, Value};
use std::sync::atomic::AtomicU32;

const POISON: u64 = 0xbadc_0ffe_2468_1357;
const CANARY: u64 = 0x51a7_6d9b_2468_1357;
const SHAPE_ID_BYTE: u32 = 160;
const RECEIVER: usize = 512;
const HOLDER: usize = 1024;
const RECEIVER_SHAPE: usize = 1536;
const HOLDER_SHAPE: usize = 2048;
const CHILD: usize = 2560;
const ROOT: usize = 3072;
const SLAB: usize = 3584;

fn snapshot() -> JitCompileSnapshot {
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::ReturnUndefined, &[]);
    ExecutionContext::from_module(
        BytecodeModule {
            module: "property-action-geometry.js".into(),
            template_sites: vec![],
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".into(),
                locals: 1,
                code: code.finish(),
                ..Function::default()
            }],
            constants: vec![],
            module_resolutions: vec![],
            module_inits: vec![],
            function_source: None,
        },
        Default::default(),
    )
    .expect("verified layout")
    .jit_compile_snapshot(0)
    .map(|mut view| {
        // CodeBlock snapshots leave allocation geometry unset. This host model
        // explicitly separates header/type, shape/exotic/slab and inline banks.
        view.object_shape_byte = 8;
        view.object_exotic_handle_byte = 16;
        view.shape_state_byte = 8;
        view.shape_inline_capacity_byte = 9;
        view.shape_prototype_byte = 16;
        // The real layout: shape, slab handle, exotic handle, then inline words.
        assert!(view.field_layout.slab_handle_byte >= view.object_shape_byte + 4);
        assert!(view.field_layout.slab_handle_byte + 4 <= view.object_exotic_handle_byte);
        assert!(view.object_exotic_handle_byte + 4 <= view.field_layout.inline_values_byte);
        view
    })
    .expect("VM source metadata and explicit host geometry")
}

fn put<T: Copy>(arena: &mut [u64], byte: usize, value: T) {
    assert!(byte + std::mem::size_of::<T>() <= std::mem::size_of_val(arena));
    // SAFETY: initialized aligned host words cover every DTO scalar. Byte-sized
    // fields can be unaligned; no reference to a fabricated VM body is formed.
    unsafe {
        arena
            .as_mut_ptr()
            .cast::<u8>()
            .add(byte)
            .cast::<T>()
            .write_unaligned(value)
    };
}
fn get<T: Copy>(arena: &[u64], byte: usize) -> T {
    assert!(byte + std::mem::size_of::<T>() <= std::mem::size_of_val(arena));
    // SAFETY: same initialized host arena as put; no collector consumes it.
    unsafe {
        arena
            .as_ptr()
            .cast::<u8>()
            .add(byte)
            .cast::<T>()
            .read_unaligned()
    }
}
fn offset(arena: &[u64], byte: usize) -> u32 {
    (arena.as_ptr() as usize + byte) as u32
}
fn address(arena: &[u64], byte: usize) -> u64 {
    (arena.as_ptr() as usize + byte) as u64
}
fn put_offset(arena: &mut [u64], field: usize, target: usize) {
    let compressed = offset(arena, target);
    put(arena, field, compressed);
}

fn prepare(arena: &mut [u64], view: &mut JitCompileSnapshot, proof: &AtomicU32) {
    assert!(
        view.shape_state_byte < SHAPE_ID_BYTE && view.shape_inline_capacity_byte < SHAPE_ID_BYTE
    );
    let first = address(arena, 0) & !u64::from(u32::MAX);
    assert_eq!(first, address(arena, SLAB + 256) & !u64::from(u32::MAX));
    view.cage_base = first as usize;
    for object in [RECEIVER, HOLDER] {
        put(arena, object, u64::from(OBJECT_BODY_TYPE_TAG));
    }
    for (object, shape) in [(RECEIVER, RECEIVER_SHAPE), (HOLDER, HOLDER_SHAPE)] {
        put_offset(arena, object + view.object_shape_byte as usize, shape);
    }
    for (shape, id) in [(RECEIVER_SHAPE, 7u64), (HOLDER_SHAPE, 8), (CHILD, 9)] {
        put(arena, shape + SHAPE_ID_BYTE as usize, id);
        put(arena, shape + view.shape_inline_capacity_byte as usize, 2u8);
        put(
            arena,
            shape + view.shape_state_byte as usize,
            ShapeState::EXTENSIBLE_MASK,
        );
    }
    put_offset(arena, ROOT + view.shape_prototype_byte as usize, HOLDER);
    put(
        arena,
        HOLDER + view.field_layout.inline_values_byte as usize,
        Value::number_i32(77).to_bits(),
    );
    put(
        arena,
        RECEIVER + view.field_layout.inline_values_byte as usize,
        Value::number_i32(11).to_bits(),
    );
    put(
        arena,
        RECEIVER + view.field_layout.inline_values_byte as usize + 8,
        Value::UNDEFINED.to_bits(),
    );
    // Atom zero is the first real interned name. The first two ways collide
    // in one component; only the third full key matches.
    put(arena, 0, 7u64);
    put(arena, 8, 123u32);
    put(arena, 64, 123u64);
    put(arena, 72, 0u32);
    let row = 128;
    put(arena, row, 7u64);
    put(arena, row + 8, 0u32);
    put(arena, row + 12, PropertyLoadAction::InheritedData as u8);
    put(arena, row + 13, PropertyStoreAction::AddOwn as u8);
    put(arena, row + 14, 0u16);
    put(arena, row + 16, 1u16);
    put(arena, row + 18, true);
    put_offset(arena, row + 20, HOLDER_SHAPE);
    put_offset(arena, row + 24, ROOT);
    put_offset(arena, row + 28, CHILD);
    put(arena, row + 32, 8u64);
    put(arena, row + 40, 9u64);
    put(arena, row + 48, proof as *const AtomicU32 as usize);
    put(arena, row + 56, proof as *const AtomicU32 as usize);
    view.property_action_cache = Some(JitPropertyActionCache {
        table_addr: arena.as_ptr() as usize,
        entry_bytes: 64,
        set_mask: 0,
        ways: 4,
        receiver_shape_id_byte: 0,
        atom_byte: 8,
        load_action_byte: 12,
        store_action_byte: 13,
        load_slot_byte: 14,
        store_slot_byte: 16,
        load_writable_byte: 18,
        holder_shape_byte: 20,
        holder_root_byte: 24,
        target_shape_byte: 28,
        holder_shape_id_byte: 32,
        target_shape_id_byte: 40,
        load_validity_byte: 48,
        store_validity_byte: 56,
        shape_id_byte: SHAPE_ID_BYTE,
        hash_shape_multiplier: 0x9e37_79b9_7f4a_7c15,
        hash_atom_multiplier: 0xc2b2_ae3d_27d4_eb4f,
        hash_shift: 32,
    });
}

fn executable(view: &JitCompileSnapshot, store: bool, alias: bool) -> CompiledCode {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let mut relocations = RelocationCapture::default();
    dynasm!(ops ; .arch aarch64 ; mov x12, x0 ; mov x9, x1 ; mov x3, x2);
    for register in [4, 5, 6] {
        emit_load_u64(&mut ops, register, CANARY);
    }
    emit_load_u64(&mut ops, 7, CANARY);
    dynasm!(ops ; .arch aarch64 ; fmov d0, x7);
    if store {
        emit_store(
            &mut ops,
            &mut relocations,
            view,
            AtomOperand::Immediate(0),
            [12, 9],
            [10, 11, 13, 15],
            miss,
        );
        dynasm!(ops ; .arch aarch64 ; mov x0, #1);
    } else if alias {
        dynasm!(ops ; .arch aarch64 ; mov x9, x12);
        emit_load(
            &mut ops,
            &mut relocations,
            view,
            AtomOperand::Immediate(0),
            9,
            [10, 11, 13, 15],
            9,
            miss,
        );
        dynasm!(ops ; .arch aarch64 ; mov x0, x9);
    } else {
        emit_load(
            &mut ops,
            &mut relocations,
            view,
            AtomOperand::Immediate(0),
            12,
            [10, 11, 13, 15],
            0,
            miss,
        );
    }
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>miss);
    emit_load_u64(&mut ops, 0, POISON);
    dynasm!(ops ; .arch aarch64 ; =>done
        ; stp x4, x5, [x3] ; str x6, [x3, #16] ; str d0, [x3, #24]
        ; str x12, [x3, #32] ; ret);
    CompiledCode::new(ops.finalize().unwrap(), entry)
}
fn run(code: &CompiledCode, arena: &mut [u64], value: Value) -> u64 {
    let receiver = address(arena, RECEIVER);
    let mut observed = [0u64; 5];
    // SAFETY: this allocation-free emitted leaf has exactly this platform ABI;
    // code and all scalar inputs remain alive through the synchronous call.
    let call: extern "C" fn(u64, u64, *mut u64) -> u64 =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let result = call(receiver, value.to_bits(), observed.as_mut_ptr());
    assert_eq!(observed, [CANARY, CANARY, CANARY, CANARY, receiver]);
    result
}

#[test]
fn emitted_actions_keep_inherited_reads_append_slots_and_pre_effect_misses_independent() {
    let proof = AtomicU32::new(1);
    let mut arena = vec![0u64; 640];
    let mut view = snapshot();
    prepare(&mut arena, &mut view, &proof);
    let before = arena.clone();
    for alias in [false, true] {
        let code = executable(&view, false, alias);
        assert_eq!(
            run(&code, &mut arena, Value::UNDEFINED),
            Value::number_i32(77).to_bits()
        );
        assert_eq!(arena, before);
    }
    put(&mut arena, 140, PropertyLoadAction::OwnData as u8);
    put(&mut arena, 141, PropertyStoreAction::Unknown as u8);
    put(&mut arena, 146, false);
    put_offset(&mut arena, 148, RECEIVER_SHAPE);
    put(&mut arena, 160, 7u64);
    let own = executable(&view, false, true);
    let before_own = arena.clone();
    assert_eq!(
        run(&own, &mut arena, Value::UNDEFINED),
        Value::number_i32(11).to_bits()
    );
    assert_eq!(arena, before_own);
    // The same inherited action must decline a holder that is no longer ordinary.
    prepare(&mut arena, &mut view, &proof);
    put(
        &mut arena,
        HOLDER_SHAPE + view.shape_state_byte as usize,
        ShapeState::DICTIONARY_MASK,
    );
    let held = executable(&view, false, false);
    let before_held = arena.clone();
    assert_eq!(run(&held, &mut arena, Value::UNDEFINED), POISON);
    assert_eq!(arena, before_held);
    prepare(&mut arena, &mut view, &proof);
    let code = executable(&view, true, false);
    assert_eq!(run(&code, &mut arena, Value::number_i32(999)), 1);
    assert_eq!(
        get::<u64>(
            &arena,
            RECEIVER + view.field_layout.inline_values_byte as usize + 8
        ),
        Value::number_i32(999).to_bits()
    );
    assert_eq!(
        get::<u64>(
            &arena,
            HOLDER + view.field_layout.inline_values_byte as usize
        ),
        Value::number_i32(77).to_bits()
    );
    assert_eq!(
        get::<u32>(&arena, RECEIVER + view.object_shape_byte as usize),
        offset(&arena, CHILD)
    );

    for failure in 0..9 {
        prepare(&mut arena, &mut view, &proof);
        match failure {
            0 => put(&mut arena, 136, 10u32), // full key mismatch
            1 => put(&mut arena, 141, PropertyStoreAction::Unknown as u8),
            2 => put(&mut arena, 156, 0u32), // runtime-only transition
            3 => proof.store(0, std::sync::atomic::Ordering::Relaxed),
            4 => put(
                &mut arena,
                RECEIVER_SHAPE + view.shape_state_byte as usize,
                ShapeState::PROTOTYPE_MASK | ShapeState::EXTENSIBLE_MASK,
            ),
            5 => put(
                &mut arena,
                RECEIVER_SHAPE + view.shape_state_byte as usize,
                0u8,
            ),
            6 => put(&mut arena, CHILD + SHAPE_ID_BYTE as usize, 123u64),
            7 => {
                put(&mut arena, 144, 2u16);
                put(
                    &mut arena,
                    RECEIVER + view.field_layout.slab_handle_byte as usize,
                    0u32,
                );
            }
            8 => put(
                &mut arena,
                RECEIVER_SHAPE + view.shape_state_byte as usize,
                ShapeState::OPAQUE_LOOKUP_MASK | ShapeState::EXTENSIBLE_MASK,
            ),
            _ => unreachable!(),
        }
        let before = arena.clone();
        let code = executable(&view, true, false);
        assert_eq!(
            run(&code, &mut arena, Value::number_i32(999)),
            POISON,
            "failure {failure}"
        );
        assert_eq!(arena, before, "pre-effect failure {failure}");
        proof.store(1, std::sync::atomic::Ordering::Relaxed);
    }
    prepare(&mut arena, &mut view, &proof);
    put(&mut arena, 140, PropertyLoadAction::Unknown as u8);
    put(&mut arena, 141, PropertyStoreAction::OwnWritable as u8);
    put(&mut arena, 144, 2u16);
    put(
        &mut arena,
        RECEIVER_SHAPE + view.shape_state_byte as usize,
        0u8,
    );
    let slab = offset(&arena, SLAB);
    put(
        &mut arena,
        RECEIVER + view.field_layout.slab_handle_byte as usize,
        slab,
    );
    put(
        &mut arena,
        SLAB + view.field_layout.slab_capacity_byte as usize,
        1u32,
    );
    let shape_before = get::<u32>(&arena, RECEIVER + view.object_shape_byte as usize);
    let code = executable(&view, true, false);
    assert_eq!(run(&code, &mut arena, Value::number_i32(555)), 1);
    assert_eq!(
        get::<u64>(&arena, SLAB + view.field_layout.slab_words_byte as usize),
        Value::number_i32(555).to_bits()
    );
    assert_eq!(
        get::<u32>(&arena, RECEIVER + view.object_shape_byte as usize),
        shape_before
    );
    put(
        &mut arena,
        SLAB + view.field_layout.slab_capacity_byte as usize,
        0u32,
    );
    let before = arena.clone();
    assert_eq!(run(&code, &mut arena, Value::number_i32(999)), POISON);
    assert_eq!(arena, before);
}
