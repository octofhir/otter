//! Executable native global-read guards over explicit host model storage.
//!
//! # Contents
//! - Complete lexical values, TDZ, source relocation and realm refusal.
//! - Live global aliases, ordinary/dictionary identities and field-bank bounds.
//! - Declared two-GP geometry with preserved GP/FP canaries and pre-effect exits.
//!
//! # Invariants
//! These initialized host words never enter the collector. The model explicitly
//! assigns nonoverlapping snapshot offsets; actual FieldLayout and VM ABI
//! context offsets remain authoritative. A changed host alias is not a moving
//! GC observation. Runtime source/realm/generation tests own actual collection.

use super::*;
use crate::CompiledCode;
use otter_vm::Value;

const POISON: u64 = 0xbadc_0ffe_2468_1357;
const CANARY: u64 = 0x51a7_6d9b_2468_1357;
const GLOBAL: u32 = 256;
const NEXT_GLOBAL: u32 = 512;
const SHAPE: u32 = 768;
const EXOTIC: u32 = 1024;
const SLAB: u32 = 1280;
const CELL: u32 = 1536;
const BYTE_PC: u32 = 29;
const REALM: u32 = 7;
const EPOCH: u64 = 0xdead_beef_89ab_cdef;
const LAYOUT: u32 = 69;

fn put<T: Copy>(words: &mut [u64], byte: usize, value: T) {
    assert!(byte + std::mem::size_of::<T>() <= std::mem::size_of_val(words));
    // SAFETY: owned initialized aligned host words cover the scalar. No VM
    // body reference is fabricated and byte fields can be unaligned.
    unsafe {
        words
            .as_mut_ptr()
            .cast::<u8>()
            .add(byte)
            .cast::<T>()
            .write_unaligned(value)
    };
}

fn snapshot(cage: &mut [u64]) -> JitCompileSnapshot {
    let mut view = JitCompileSnapshot::without_feedback(41, 0, 1, vec![]);
    view.cage_base = cage.as_mut_ptr() as usize;
    view.object_shape_byte = 8;
    view.object_exotic_handle_byte = 16;
    view.shape_state_byte = 8;
    view.shape_inline_capacity_byte = 9;
    view.exotic_dictionary_layout_byte = 8;
    view.global_lexical_value_byte = 8;
    view.literal_allocations.realm_id = REALM;
    // The actual field owner is intentionally retained. Its object slab word
    // cannot alias the model's shape/exotic words or inline values.
    // The real layout: shape, slab handle, exotic handle, then inline words.
    assert!(view.field_layout.slab_handle_byte >= view.object_shape_byte + 4);
    assert!(view.field_layout.slab_handle_byte + 4 <= view.object_exotic_handle_byte);
    assert!(view.object_exotic_handle_byte + 4 <= view.field_layout.inline_values_byte);
    assert!(view.field_layout.slab_words_byte >= view.field_layout.slab_capacity_byte + 4);
    for object in [GLOBAL, NEXT_GLOBAL] {
        put(cage, (object + view.object_shape_byte) as usize, SHAPE);
        put(
            cage,
            (object + view.object_exotic_handle_byte) as usize,
            EXOTIC,
        );
        put(
            cage,
            (object + view.field_layout.slab_handle_byte) as usize,
            SLAB,
        );
    }
    put(
        cage,
        (SHAPE + view.shape_state_byte) as usize,
        ShapeState::ORDINARY.bits(),
    );
    put(
        cage,
        (SHAPE + view.shape_inline_capacity_byte) as usize,
        2u8,
    );
    put(
        cage,
        (EXOTIC + view.exotic_dictionary_layout_byte) as usize,
        LAYOUT,
    );
    put(
        cage,
        (SLAB + view.field_layout.slab_capacity_byte) as usize,
        2u32,
    );
    view
}

struct Activation {
    context: Box<[u64]>,
    thread: Box<[u64]>,
    global: Box<u32>,
    epoch: Box<u64>,
    realm: Box<u32>,
}
impl Activation {
    fn new() -> Self {
        let context_bytes = THREAD_OFFSET.max(GLOBAL_THIS_OFFSET_PTR_OFFSET) as usize + 8;
        let thread_bytes = VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET
            .max(VM_THREAD_ACTIVE_REALM_CELL_OFFSET) as usize
            + 8;
        let mut this = Self {
            context: vec![0u64; context_bytes.div_ceil(8)].into_boxed_slice(),
            thread: vec![0u64; thread_bytes.div_ceil(8)].into_boxed_slice(),
            global: Box::new(GLOBAL),
            epoch: Box::new(EPOCH),
            realm: Box::new(REALM),
        };
        let thread = this.thread.as_mut_ptr() as usize;
        let global = (&mut *this.global) as *mut u32 as usize;
        let epoch = (&mut *this.epoch) as *mut u64 as usize;
        let realm = (&mut *this.realm) as *mut u32 as usize;
        put(&mut this.context, THREAD_OFFSET as usize, thread);
        put(
            &mut this.context,
            GLOBAL_THIS_OFFSET_PTR_OFFSET as usize,
            global,
        );
        put(
            &mut this.thread,
            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
            epoch,
        );
        put(
            &mut this.thread,
            VM_THREAD_ACTIVE_REALM_CELL_OFFSET as usize,
            realm,
        );
        this
    }
    fn realm_pointer(&mut self, live: bool) {
        let pointer = if live {
            (&mut *self.realm) as *mut u32 as usize
        } else {
            0
        };
        put(
            &mut self.thread,
            VM_THREAD_ACTIVE_REALM_CELL_OFFSET as usize,
            pointer,
        );
    }
}

fn executable(view: &JitCompileSnapshot, proof: BindingHitProof, destination: u8) -> CompiledCode {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let mut relocations = RelocationCapture::new(true);
    dynasm!(ops ; .arch aarch64 ; stp x20, x30, [sp, #-16]! ; mov x20, x0 ; mov x3, x1);
    for register in [2, 4, 5, 6] {
        emit_load_u64(&mut ops, register, CANARY);
    }
    emit_load_u64(&mut ops, 7, CANARY);
    dynasm!(ops ; .arch aarch64 ; fmov d0, x7);
    emit_global_read(
        &mut ops,
        &mut relocations,
        view,
        proof,
        BYTE_PC,
        destination,
        [12, 13],
        miss,
    )
    .unwrap();
    dynasm!(ops ; .arch aarch64 ; mov x0, X(destination) ; b =>done ; =>miss);
    emit_load_u64(&mut ops, 0, POISON);
    dynasm!(ops ; .arch aarch64 ; =>done
        ; stp x2, x4, [x3] ; stp x5, x6, [x3, #16] ; str d0, [x3, #32]
        ; ldp x20, x30, [sp], #16 ; ret);
    let buffer = ops.finalize().unwrap();
    if matches!(proof, BindingHitProof::GlobalLexical { .. }) {
        let rendered = relocations.render(&buffer).unwrap();
        let json: serde_json::Value = serde_json::from_str(&rendered.json).unwrap();
        let records = json["relocations"]
            .as_array()
            .expect("sole current relocation schema");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["target"]["kind"], "globalLexicalCell");
        assert_eq!(records[0]["target"]["functionId"], view.code_block.id);
        assert_eq!(records[0]["target"]["bytePc"], BYTE_PC);
    }
    CompiledCode::new(buffer, entry)
}
fn run(code: &CompiledCode, activation: &mut Activation, cage: &mut [u64]) -> u64 {
    let mut observed = [0u64; 5];
    let before = cage.to_vec();
    // SAFETY: this allocation-free native leaf has this exact platform ABI.
    // All explicit context/cell/bank storage and code remain live; it reads no
    // fabricated VM references, calls nothing and preserves x20/LR/SP.
    let call: unsafe extern "C" fn(*mut u64, *mut u64) -> u64 =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let result = unsafe { call(activation.context.as_mut_ptr(), observed.as_mut_ptr()) };
    assert_eq!(observed, [CANARY; 5]);
    assert_eq!(
        cage,
        before.as_slice(),
        "pre-effect reader never writes storage"
    );
    result
}

#[test]
fn lexical_reader_keeps_live_complete_values_tdz_source_and_realm() {
    let mut cage = vec![0u64; 256];
    let view = snapshot(&mut cage);
    let mut activation = Activation::new();
    let proof = BindingHitProof::GlobalLexical {
        cell_offset: CELL,
        writable: true,
    };
    for destination in [0, 12, 13] {
        let code = executable(&view, proof, destination);
        for value in [
            Value::UNDEFINED.to_bits(),
            Value::number_i32(37).to_bits(),
            0x1234_5678_89ab_cdef,
        ] {
            put(
                &mut cage,
                (CELL + view.global_lexical_value_byte) as usize,
                value,
            );
            assert_eq!(run(&code, &mut activation, &mut cage), value);
        }
        put(
            &mut cage,
            (CELL + view.global_lexical_value_byte) as usize,
            VALUE_HOLE,
        );
        assert_eq!(run(&code, &mut activation, &mut cage), POISON);
        put(
            &mut cage,
            (CELL + view.global_lexical_value_byte) as usize,
            Value::number_i32(37).to_bits(),
        );
        *activation.realm = REALM + 1;
        assert_eq!(run(&code, &mut activation, &mut cage), POISON);
        *activation.realm = REALM;
        activation.realm_pointer(false);
        assert_eq!(run(&code, &mut activation, &mut cage), POISON);
        activation.realm_pointer(true);
    }
}

#[test]
fn global_reader_proves_current_alias_epoch_identity_state_and_bank_bounds() {
    let mut cage = vec![0u64; 256];
    let view = snapshot(&mut cage);
    let mut activation = Activation::new();
    let inline = FieldLocation::inline(1);
    let suffix = FieldLocation::overflow(1);
    for dictionary in [false, true] {
        put(
            &mut cage,
            (SHAPE + view.shape_state_byte) as usize,
            if dictionary {
                ShapeState::DICTIONARY_MASK
            } else {
                ShapeState::ORDINARY.bits()
            },
        );
        for field in [inline, suffix] {
            let proof = BindingHitProof::GlobalObject {
                shape: u64::from(if dictionary { LAYOUT } else { SHAPE }),
                dictionary,
                field,
                global_lexical_epoch: EPOCH,
                writable: true,
            };
            let code = executable(&view, proof, 12);
            let current = if field.is_inline() {
                GLOBAL + view.field_layout.inline_values_byte
            } else {
                SLAB + view.field_layout.slab_words_byte
            } + field.byte_offset();
            put(&mut cage, current as usize, Value::number_i32(77).to_bits());
            assert_eq!(
                run(&code, &mut activation, &mut cage),
                Value::number_i32(77).to_bits()
            );
            if field.is_inline() {
                let next = NEXT_GLOBAL + view.field_layout.inline_values_byte + field.byte_offset();
                put(&mut cage, next as usize, Value::number_i32(88).to_bits());
                *activation.global = NEXT_GLOBAL;
                assert_eq!(
                    run(&code, &mut activation, &mut cage),
                    Value::number_i32(88).to_bits()
                );
                *activation.global = GLOBAL;
            }
            *activation.epoch = EPOCH ^ 1;
            assert_eq!(run(&code, &mut activation, &mut cage), POISON);
            *activation.epoch = EPOCH;
            *activation.realm = REALM + 1;
            assert_eq!(run(&code, &mut activation, &mut cage), POISON);
            *activation.realm = REALM;
            *activation.global = 0;
            assert_eq!(run(&code, &mut activation, &mut cage), POISON);
            *activation.global = GLOBAL;
            if dictionary {
                put(
                    &mut cage,
                    (EXOTIC + view.exotic_dictionary_layout_byte) as usize,
                    LAYOUT + 1,
                );
                assert_eq!(run(&code, &mut activation, &mut cage), POISON);
                put(
                    &mut cage,
                    (EXOTIC + view.exotic_dictionary_layout_byte) as usize,
                    LAYOUT,
                );
                for state in [
                    ShapeState::ORDINARY.bits(),
                    ShapeState::DICTIONARY_MASK | ShapeState::OPAQUE_LOOKUP_MASK,
                    ShapeState::DICTIONARY_MASK | ShapeState::PROVISIONAL_MASK,
                ] {
                    put(&mut cage, (SHAPE + view.shape_state_byte) as usize, state);
                    assert_eq!(run(&code, &mut activation, &mut cage), POISON);
                }
                put(
                    &mut cage,
                    (SHAPE + view.shape_state_byte) as usize,
                    ShapeState::DICTIONARY_MASK,
                );
            } else {
                put(
                    &mut cage,
                    (GLOBAL + view.object_shape_byte) as usize,
                    SHAPE + 64,
                );
                assert_eq!(run(&code, &mut activation, &mut cage), POISON);
                put(&mut cage, (GLOBAL + view.object_shape_byte) as usize, SHAPE);
            }
            if field.is_inline() {
                put(
                    &mut cage,
                    (SHAPE + view.shape_inline_capacity_byte) as usize,
                    1u8,
                );
                assert_eq!(run(&code, &mut activation, &mut cage), POISON);
                put(
                    &mut cage,
                    (SHAPE + view.shape_inline_capacity_byte) as usize,
                    2u8,
                );
            } else {
                put(
                    &mut cage,
                    (GLOBAL + view.field_layout.slab_handle_byte) as usize,
                    0u32,
                );
                assert_eq!(run(&code, &mut activation, &mut cage), POISON);
                put(
                    &mut cage,
                    (GLOBAL + view.field_layout.slab_handle_byte) as usize,
                    SLAB,
                );
                put(
                    &mut cage,
                    (SLAB + view.field_layout.slab_capacity_byte) as usize,
                    1u32,
                );
                assert_eq!(run(&code, &mut activation, &mut cage), POISON);
                put(
                    &mut cage,
                    (SLAB + view.field_layout.slab_capacity_byte) as usize,
                    2u32,
                );
            }
        }
    }
}
