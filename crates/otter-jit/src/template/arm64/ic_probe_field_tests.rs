//! Executable proofs of per-program field-bank selection for named stores.
//!
//! # Contents
//! - One native guard containing inline existing-slot and suffix append programs.
//! - Exact register, field, child-shape publication and pre-effect miss checks.
//!
//! # Invariants
//! - Aligned host buffers own every byte read or written by generated code;
//!   they never enter the GC or execute a VM/barrier call.
//! - Object/header/slab offsets come from the current production layout.
//!   Shape tokens address separate owned records containing the sole state byte;
//!   their offsets follow the snapshot's production shape geometry.
//! - The suffix program retains a live owned chain-validity word. No baked
//!   address outlives its owner or the finalized executable mapping.
//! - Whole-buffer comparison permits only the selected value word and, for
//!   an append, the receiver's child-shape word to change.
//!
//! # See also
//! - [`super::emit_property_ic_store_guard`] is the production emitter under test.
//! - `otter_vm::object::FieldLocation` owns bank-relative slot numbering.

use std::sync::atomic::{AtomicU32, Ordering};

use dynasmrt::{DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};
use otter_vm::{
    JitCompileSnapshot, Value,
    jit::{JitCacheIrOp, JitCacheIrProgram, JitEmptyObjectAllocationPlan, JitPrototypeValidity},
    object::{FieldLayout, FieldLocation, ShapeState},
};

use crate::{CompiledCode, artifact::relocation::RelocationCapture};

const POISON: u64 = 0xcafe_1357_2468_abcd;
const INLINE_SHAPE: u32 = 0;
const SUFFIX_SHAPE: u32 = 1;
const CHILD_SHAPE: u32 = 2;
const OTHER_SHAPE: u32 = 3;
const SHAPES_BYTE: usize = 1024;
const SHAPE_STRIDE: usize = 128;
const OBJECT_BYTE: usize = 8;
const SLAB_BYTE: usize = 80 * 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct GuardOutput {
    parent: u64,
    bank: u64,
    byte_offset: u64,
    child_shape: u64,
}

impl GuardOutput {
    const POISONED: Self = Self {
        parent: POISON,
        bank: POISON,
        byte_offset: POISON,
        child_shape: POISON,
    };
}

struct Fixture {
    words: Vec<u64>,
    view: JitCompileSnapshot,
    object_plan: JitEmptyObjectAllocationPlan,
}

impl Fixture {
    fn new() -> Self {
        let words = vec![POISON; 256];
        let inline_shape = (words.as_ptr() as usize + SHAPES_BYTE) as u32;
        let object_plan = JitEmptyObjectAllocationPlan::new(inline_shape);
        let object = words.as_ptr() as usize + OBJECT_BYTE;
        let slab = words.as_ptr() as usize + SLAB_BYTE;
        let final_shape_byte = words.as_ptr() as usize + SHAPES_BYTE + 4 * SHAPE_STRIDE - 1;
        assert_eq!(object >> 32, slab >> 32, "fixture cells share a cage base");
        assert_eq!(
            object >> 32,
            final_shape_byte >> 32,
            "owned shape records share that cage"
        );
        assert_eq!(object as u64 & otter_vm::value::tag::NOT_CELL_MASK, 0);
        let mut view = JitCompileSnapshot::without_feedback(0, 0, 0, Vec::new());
        view.cage_base = object & !0xffff_ffff;
        view.object_shape_byte = object_plan.shape_byte;
        assert!((view.shape_state_byte as usize) < SHAPE_STRIDE);
        assert_eq!(view.field_layout, FieldLayout::current());
        assert!(OBJECT_BYTE + object_plan.cell_bytes as usize <= SLAB_BYTE);
        assert!(SLAB_BYTE + view.field_layout.slab_words_byte as usize + 2 * 8 <= words.len() * 8);
        Self {
            words,
            view,
            object_plan,
        }
    }

    fn object(&self) -> usize {
        self.words.as_ptr() as usize + OBJECT_BYTE
    }

    fn bank(&self, field: FieldLocation) -> usize {
        self.words.as_ptr() as usize
            + if field.is_inline() {
                OBJECT_BYTE + self.view.field_layout.inline_values_byte as usize
            } else {
                SLAB_BYTE + self.view.field_layout.slab_words_byte as usize
            }
    }

    fn field_byte(&self, field: FieldLocation) -> usize {
        self.bank(field) - self.words.as_ptr() as usize + field.byte_offset() as usize
    }

    fn put_u32(&mut self, byte: usize, value: u32) {
        assert!(byte.is_multiple_of(4) && byte + 4 <= self.words.len() * 8);
        // SAFETY: this aligned four-byte word is inside the owned raw buffer.
        // No Rust payload or collector owns the synthetic cell bytes.
        unsafe {
            self.words
                .as_mut_ptr()
                .cast::<u8>()
                .add(byte)
                .cast::<u32>()
                .write(value)
        };
    }

    fn shape(&self, index: u32) -> u32 {
        assert!(index < 4);
        (self.words.as_ptr() as usize + SHAPES_BYTE + index as usize * SHAPE_STRIDE) as u32
    }

    fn set_shape(&mut self, index: u32) {
        self.put_u32(
            OBJECT_BYTE + self.view.object_shape_byte as usize,
            self.shape(index),
        );
    }

    fn set_suffix(&mut self, present: bool, capacity: u32) {
        let handle = if present {
            (self.words.as_ptr() as usize + SLAB_BYTE) as u32
        } else {
            0
        };
        self.put_u32(
            OBJECT_BYTE + self.view.field_layout.slab_handle_byte as usize,
            handle,
        );
        self.put_u32(
            SLAB_BYTE + self.view.field_layout.slab_capacity_byte as usize,
            capacity,
        );
    }

    fn set_state(&mut self, index: u32, state: ShapeState) {
        let byte =
            SHAPES_BYTE + index as usize * SHAPE_STRIDE + self.view.shape_state_byte as usize;
        assert!(index < 4 && byte < self.words.len() * 8);
        // SAFETY: this mock immutable-shape record is inside the owned arena.
        // It is prepared before execution and never mutated by the emitter.
        unsafe {
            self.words
                .as_mut_ptr()
                .cast::<u8>()
                .add(byte)
                .write(state.bits())
        };
    }

    fn reset(&mut self, shape: u32) {
        self.words.fill(POISON);
        for index in 0..4 {
            self.set_state(index, ShapeState::ORDINARY);
        }
        self.words[OBJECT_BYTE / 8] = self.object_plan.header_word;
        self.set_shape(shape);
        self.set_suffix(true, 2);
        for index in 0..4 {
            let byte = self.field_byte(FieldLocation::inline(index));
            self.words[byte / 8] = Value::number_i32(100 + index as i32).to_bits();
        }
        for index in 0..2 {
            let byte = self.field_byte(FieldLocation::overflow(index));
            self.words[byte / 8] = Value::number_i32(200 + index as i32).to_bits();
        }
    }
}

fn compile_probe(fixture: &Fixture, validity: &AtomicU32, suffix_first: bool) -> CompiledCode {
    let view = &fixture.view;
    let inline = FieldLocation::inline(1);
    let suffix = FieldLocation::overflow(1);
    let existing = JitCacheIrProgram {
        ops: vec![
            JitCacheIrOp::GuardShape {
                object: 0,
                shape: fixture.shape(INLINE_SHAPE),
            },
            JitCacheIrOp::GuardAtomSlot {
                object: 0,
                atom: 1,
                field: inline,
                writable: true,
            },
            JitCacheIrOp::StoreField {
                object: 0,
                field: inline,
            },
        ]
        .into_boxed_slice(),
    };
    let append = JitCacheIrProgram {
        ops: vec![
            JitCacheIrOp::GuardShape {
                object: 0,
                shape: fixture.shape(SUFFIX_SHAPE),
            },
            JitCacheIrOp::GuardPrototypeValidity {
                validity: JitPrototypeValidity {
                    address: validity as *const AtomicU32 as usize,
                    identity: u64::from(fixture.shape(SUFFIX_SHAPE)),
                },
            },
            JitCacheIrOp::GuardExtensible {
                object: 0,
                field: suffix,
            },
            JitCacheIrOp::StoreField {
                object: 0,
                field: suffix,
            },
            JitCacheIrOp::PublishShape {
                object: 0,
                shape: fixture.shape(CHILD_SHAPE),
            },
        ]
        .into_boxed_slice(),
    };
    let programs = if suffix_first {
        [append, existing]
    } else {
        [existing, append]
    };
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let mut relocations = RelocationCapture::default();
    super::emit_property_ic_store_guard(
        &mut ops,
        &mut relocations,
        view,
        Some(&programs),
        |ops, register| {
            dynasm!(ops ; .arch aarch64 ; mov X(register), x0);
            Ok(())
        },
        0,
        0,
        miss,
    )
    .expect("mixed bank programs lower");
    // The caller observes the production helper's exact outgoing registers,
    // then commits one primitive word; neither path calls a barrier or the VM.
    dynasm!(ops ; .arch aarch64
        ; stp x12, x13, [x2]
        ; stp x17, x16, [x2, #16]
        ; str x1, [x13, x17]
        ; movz x0, 1
        ; ret
        ; =>miss
        ; mov x0, xzr
        ; ret);
    CompiledCode::new(ops.finalize().unwrap(), entry)
}

fn run_probe(code: &CompiledCode, fixture: &mut Fixture, output: &mut GuardOutput) -> u64 {
    // SAFETY: the private three-word C ABI uses only caller-saved registers,
    // preserves SP/LR, and accesses the owned raw cells/output and retained
    // validity word. The code mapping and every baked address remain live.
    let probe: unsafe extern "C" fn(usize, u64, *mut GuardOutput) -> u64 =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    // SAFETY: same live fixture/mapping contract as the transmute above.
    unsafe { probe(fixture.object(), Value::number_i32(719).to_bits(), output) }
}

#[test]
fn mixed_store_programs_select_their_bank_and_publish_only_on_a_match() {
    let mut fixture = Fixture::new();
    let validity = AtomicU32::new(1);
    for suffix_first in [false, true] {
        let code = compile_probe(&fixture, &validity, suffix_first);
        for (shape, field, child_shape) in [
            (INLINE_SHAPE, FieldLocation::inline(1), 0),
            (SUFFIX_SHAPE, FieldLocation::overflow(1), CHILD_SHAPE),
        ] {
            fixture.reset(shape);
            let child_shape = if shape == SUFFIX_SHAPE {
                fixture.shape(child_shape)
            } else {
                0
            };
            let mut expected = fixture.words.clone();
            expected[fixture.field_byte(field) / 8] = Value::number_i32(719).to_bits();
            if child_shape != 0 {
                let byte = OBJECT_BYTE + fixture.view.object_shape_byte as usize;
                let shift = (byte % 8) * 8;
                expected[byte / 8] = (expected[byte / 8] & !(u64::from(u32::MAX) << shift))
                    | (u64::from(child_shape) << shift);
            }
            let mut output = GuardOutput::POISONED;
            assert_eq!(run_probe(&code, &mut fixture, &mut output), 1);
            assert_eq!(
                output,
                GuardOutput {
                    parent: fixture.object() as u64,
                    bank: fixture.bank(field) as u64,
                    byte_offset: u64::from(field.byte_offset()),
                    child_shape: u64::from(child_shape),
                },
                "suffix_first={suffix_first}, field={field:?}"
            );
            assert_eq!(
                fixture.words, expected,
                "only selected field/shape may change"
            );
        }
        for miss in [
            "shape",
            "missing_suffix",
            "capacity",
            "extensible",
            "validity",
            "prototype",
        ] {
            fixture.reset(SUFFIX_SHAPE);
            match miss {
                "shape" => fixture.set_shape(OTHER_SHAPE),
                "missing_suffix" => fixture.set_suffix(false, 2),
                "capacity" => fixture.set_suffix(true, 1),
                "extensible" => {
                    fixture.set_state(SUFFIX_SHAPE, ShapeState::ORDINARY.with_extensible(false))
                }
                "validity" => validity.store(0, Ordering::Release),
                "prototype" => {
                    fixture.set_state(SUFFIX_SHAPE, ShapeState::ORDINARY.with_prototype_role(true))
                }
                _ => unreachable!(),
            }
            let expected = fixture.words.clone();
            let mut output = GuardOutput::POISONED;
            assert_eq!(run_probe(&code, &mut fixture, &mut output), 0, "{miss}");
            assert_eq!(output, GuardOutput::POISONED, "{miss}");
            assert_eq!(fixture.words, expected, "{miss}: no publication or store");
            validity.store(1, Ordering::Release);
        }
    }
}
