//! Executable geometry of the shared x86-64 global binding emitter.
//!
//! # Contents
//! - Live lexical replacements, TDZ refusal and actual source relocations.
//! - Epoch/layout/state and current inline/suffix capacity checks.
//! - Register preservation and absence of effects on every pre-read miss.
//!
//! # Invariants
//! - Aligned owned words model the current VM offsets and remain stationary
//!   throughout a no-call native probe. This is encoding evidence, not a GC
//!   or realm-admission substitute for the coherent runtime gate.
//! - Both register assignments invoke the production shared read owner.
//! - Native misses retain the separate result canary and every fixture word.
//!
//! # See also
//! - [`super`] for production cell, guard and bank ownership.

use dynasmrt::{AssemblyOffset, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{
    JitCompileSnapshot,
    jit::BindingHitProof,
    object::{FieldLocation, ShapeState},
    value::tag,
};

use super::*;

const OBJECT: usize = 64;
const SHAPE: usize = 1024;
const EXOTIC: usize = 1280;
const SLAB: usize = 1536;
const NEXT_SLAB: usize = 1792;
const LEXICAL: usize = 2304;
const EPOCH: u64 = 0xdead_beef_89ab_cdef;
const LAYOUT: u32 = 0xfedc_ba98;
const FID: u32 = 1541;
const BYTE_PC: u32 = 0xfedc_ba97;
const CANARY: u64 = 0x5555_5555_5555_5555;

fn set_u32(words: &mut [u64], byte: usize, value: u32) {
    assert_eq!(byte % 4, 0);
    assert!(byte + 4 <= std::mem::size_of_val(words));
    // SAFETY: the word buffer owns this aligned, in-bounds scalar range.
    unsafe {
        words
            .as_mut_ptr()
            .cast::<u8>()
            .add(byte)
            .cast::<u32>()
            .write(value)
    };
}
fn set_u8(words: &mut [u64], byte: usize, value: u8) {
    assert!(byte < std::mem::size_of_val(words));
    // SAFETY: this byte lies inside the owned word buffer.
    unsafe { words.as_mut_ptr().cast::<u8>().add(byte).write(value) };
}
fn set_u64(words: &mut [u64], byte: usize, value: u64) {
    assert_eq!(byte % 8, 0);
    words[byte / 8] = value;
}

struct Fixture {
    cells: Vec<u64>,
    thread: Vec<u64>,
    context: Vec<u64>,
    global: Box<u32>,
    epoch: Box<u64>,
    realm: Box<u32>,
    view: JitCompileSnapshot,
}

impl Fixture {
    fn new(capacity: u8, dictionary: bool) -> Self {
        let cells = vec![CANARY; 512];
        let thread = vec![
            0_u64;
            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET.max(VM_THREAD_ACTIVE_REALM_CELL_OFFSET)
                as usize
                / 8
                + 1
        ];
        let context =
            vec![0_u64; THREAD_OFFSET.max(GLOBAL_THIS_OFFSET_PTR_OFFSET) as usize / 8 + 1];
        let mut view = JitCompileSnapshot::without_feedback(
            FID,
            3,
            3,
            vec![otter_vm::jit::JitTestInstruction::new(
                otter_bytecode::Op::ReturnUndefined,
                0,
                0,
                vec![],
            )],
        );
        view.cage_base = cells.as_ptr() as usize;
        view.literal_allocations.realm_id = 0x89ab_cdef;
        // The feedback-free fixture does not populate production geometry.
        // Give every consumed DTO field its own model offset; preserve the
        // actual shared FieldLayout and VM context offset contracts.
        view.object_shape_byte = 8;
        view.object_exotic_handle_byte = 16;
        view.shape_inline_capacity_byte = 8;
        view.shape_state_byte = 9;
        view.exotic_dictionary_layout_byte = 8;
        view.global_lexical_value_byte = 8;
        let mut f = Self {
            cells,
            thread,
            context,
            global: Box::new(OBJECT as u32),
            epoch: Box::new(EPOCH),
            realm: Box::new(0x89ab_cdef),
            view,
        };
        set_u64(
            &mut f.context,
            THREAD_OFFSET as usize,
            f.thread.as_ptr() as u64,
        );
        set_u64(
            &mut f.context,
            GLOBAL_THIS_OFFSET_PTR_OFFSET as usize,
            f.global.as_ref() as *const u32 as u64,
        );
        set_u64(
            &mut f.thread,
            VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
            f.epoch.as_ref() as *const u64 as u64,
        );
        set_u64(
            &mut f.thread,
            VM_THREAD_ACTIVE_REALM_CELL_OFFSET as usize,
            f.realm.as_ref() as *const u32 as u64,
        );
        set_u32(
            &mut f.cells,
            OBJECT + f.view.object_shape_byte as usize,
            SHAPE as u32,
        );
        set_u32(
            &mut f.cells,
            OBJECT + f.view.object_exotic_handle_byte as usize,
            EXOTIC as u32,
        );
        set_u8(
            &mut f.cells,
            SHAPE + f.view.shape_inline_capacity_byte as usize,
            capacity,
        );
        set_u8(
            &mut f.cells,
            SHAPE + f.view.shape_state_byte as usize,
            if dictionary {
                ShapeState::ORDINARY.with_dictionary(true)
            } else {
                ShapeState::ORDINARY
            }
            .bits(),
        );
        set_u32(
            &mut f.cells,
            EXOTIC + f.view.exotic_dictionary_layout_byte as usize,
            LAYOUT,
        );
        set_u32(
            &mut f.cells,
            OBJECT + f.view.field_layout.slab_handle_byte as usize,
            SLAB as u32,
        );
        for slab in [SLAB, NEXT_SLAB] {
            set_u32(
                &mut f.cells,
                slab + f.view.field_layout.slab_capacity_byte as usize,
                8,
            );
        }
        f
    }
    fn lexical(&self) -> BindingHitProof {
        BindingHitProof::GlobalLexical {
            cell_offset: LEXICAL as u32,
            writable: false,
        }
    }
    fn object(&self, dictionary: bool, field: FieldLocation) -> BindingHitProof {
        BindingHitProof::GlobalObject {
            shape: u64::from(if dictionary { LAYOUT } else { SHAPE as u32 }),
            dictionary,
            field,
            global_lexical_epoch: EPOCH,
            writable: false,
        }
    }
    fn lexical_byte(&self) -> usize {
        LEXICAL + self.view.global_lexical_value_byte as usize
    }
    fn bank_byte(&self, slab: usize, field: FieldLocation) -> usize {
        if field.is_inline() {
            OBJECT
                + self.view.field_layout.inline_values_byte as usize
                + field.byte_offset() as usize
        } else {
            slab + self.view.field_layout.slab_words_byte as usize + field.byte_offset() as usize
        }
    }
    fn run(&self, proof: BindingHitProof, temps: [u8; 2]) -> (u64, [u64; 6], String) {
        let mut ops = Assembler::new().unwrap();
        let mut relocations = RelocationCapture::new(true);
        let miss = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64
            ; push rbx ; push r12 ; push r13 ; push r14 ; push r15
            ; mov r12, rsi ; mov r15, rdi
            ; mov rbx, QWORD CANARY as i64
            ; mov r13, QWORD 0x2222_2222_2222_2222_u64 as i64
            ; mov r14, QWORD 0x3333_3333_3333_3333_u64 as i64);
        let guard_end = emit_global_read(
            &mut ops,
            &mut relocations,
            &self.view,
            proof,
            BYTE_PC,
            3,
            temps,
            miss,
        )
        .unwrap();
        assert!(guard_end > 0);
        dynasm!(ops ; .arch x64
            ; mov eax, 1 ; jmp =>done
            ; =>miss ; xor eax, eax
            ; =>done
            ; mov [r12], rbx ; mov [r12 + 8], rdi ; mov [r12 + 16], rsi
            ; mov [r12 + 24], r13 ; mov [r12 + 32], r14 ; mov [r12 + 40], r15
            ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; pop rbx ; ret);
        let buffer = ops.finalize().unwrap();
        let rendered = relocations.render(&buffer).unwrap();
        // SAFETY: the private SysV function only reads stationary owned buffers
        // and scalar owners. It saves all used nonvolatile registers and makes
        // no C/runtime/GC calls. Six output words are writable for the extent.
        let probe: unsafe extern "sysv64" fn(*const u64, *mut u64) -> u64 =
            unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
        let mut observed = [0_u64; 6];
        // SAFETY: all input pointers remain owned and unchanged during probe.
        let result = unsafe { probe(self.context.as_ptr(), observed.as_mut_ptr()) };
        assert_eq!(observed[1], self.context.as_ptr() as u64);
        assert_eq!(observed[2], observed.as_ptr() as u64);
        assert_eq!(observed[3], 0x2222_2222_2222_2222);
        assert_eq!(observed[4], 0x3333_3333_3333_3333);
        assert_eq!(observed[5], self.context.as_ptr() as u64);
        (result, observed, rendered.json)
    }
}

#[test]
fn lexical_read_is_live_source_identified_and_tdz_miss_precedes_result() {
    let mut f = Fixture::new(4, false);
    for temps in [[1, 8], [9, 2]] {
        for value in [tag::NUMBER_TAG | 317, tag::VALUE_UNDEFINED, tag::VALUE_HOLE] {
            let byte = f.lexical_byte();
            set_u64(&mut f.cells, byte, value);
            let before = f.cells.clone();
            let (result, observed, json) = f.run(f.lexical(), temps);
            assert_eq!(result, u64::from(value != tag::VALUE_HOLE));
            assert_eq!(
                observed[0],
                if value == tag::VALUE_HOLE {
                    CANARY
                } else {
                    value
                }
            );
            assert_eq!(f.cells, before);
            let json: serde_json::Value = serde_json::from_str(&json).unwrap();
            let relocations = json["relocations"].as_array().unwrap();
            assert_eq!(relocations.len(), 1);
            assert_eq!(relocations[0]["target"]["kind"], "globalLexicalCell");
            assert_eq!(relocations[0]["target"]["functionId"], FID);
            assert_eq!(relocations[0]["target"]["bytePc"], BYTE_PC);
            assert_eq!(relocations[0]["widthBits"], 64);
        }
    }
    let proof = f.lexical();
    set_u64(
        &mut f.cells,
        LEXICAL + f.view.global_lexical_value_byte as usize,
        tag::NUMBER_TAG | 411,
    );
    for missing in [false, true] {
        if missing {
            set_u64(
                &mut f.thread,
                VM_THREAD_ACTIVE_REALM_CELL_OFFSET as usize,
                0,
            );
        } else {
            *f.realm ^= 1;
        }
        let before = f.cells.clone();
        let (outcome, observed, _) = f.run(proof, [1, 8]);
        assert_eq!(outcome, 0);
        assert_eq!(
            observed[0], CANARY,
            "realm mismatch/missing precedes lexical hit"
        );
        assert_eq!(f.cells, before);
    }
    let mut ops = Assembler::new().unwrap();
    let mut relocation = RelocationCapture::new(false);
    let miss = ops.new_dynamic_label();
    f.view.cage_base = usize::MAX;
    assert!(!emit_global_cell_address(
        &mut ops,
        &mut relocation,
        &f.view,
        LEXICAL as u32,
        BYTE_PC,
        8,
        miss
    ));
    assert_eq!(
        ops.offset().0,
        0,
        "invalid address leaves existence cold-reachable"
    );
    f.view.cage_base = 0;
    assert!(!emit_global_cell_address(
        &mut ops,
        &mut relocation,
        &f.view,
        LEXICAL as u32,
        BYTE_PC,
        8,
        miss
    ));
    assert_eq!(ops.offset().0, 0);
}

#[test]
fn object_read_uses_current_epoch_layout_capacity_and_suffix_before_result() {
    for capacity in [0, 4, 64] {
        for dictionary in [false, true] {
            for field in [FieldLocation::overflow(2), FieldLocation::inline(0)] {
                if capacity == 0 && field.is_inline() {
                    continue;
                }
                let mut f = Fixture::new(capacity, dictionary);
                let byte = f.bank_byte(SLAB, field);
                set_u64(&mut f.cells, byte, tag::NUMBER_TAG | 701);
                let next_byte = f.bank_byte(NEXT_SLAB, field);
                if !field.is_inline() {
                    set_u64(&mut f.cells, next_byte, tag::NUMBER_TAG | 702);
                }
                for temps in [[1, 8], [9, 2]] {
                    let before = f.cells.clone();
                    assert_eq!(
                        f.run(f.object(dictionary, field), temps).1[0],
                        tag::NUMBER_TAG | 701
                    );
                    assert_eq!(f.cells, before);
                }
                if !field.is_inline() {
                    set_u32(
                        &mut f.cells,
                        OBJECT + f.view.field_layout.slab_handle_byte as usize,
                        NEXT_SLAB as u32,
                    );
                    assert_eq!(
                        f.run(f.object(dictionary, field), [9, 2]).1[0],
                        tag::NUMBER_TAG | 702
                    );
                }
            }
        }
    }
    for refusal in 0..13 {
        let dictionary = refusal >= 5;
        let mut f = Fixture::new(4, dictionary);
        let field = FieldLocation::overflow(2);
        let proof = f.object(dictionary, field);
        match refusal {
            0 => *f.epoch ^= 1,
            1 => set_u64(
                &mut f.thread,
                VM_THREAD_GLOBAL_LEXICAL_EPOCH_CELL_OFFSET as usize,
                0,
            ),
            2 => set_u32(
                &mut f.cells,
                OBJECT + f.view.object_shape_byte as usize,
                SHAPE as u32 + 4,
            ),
            3 => set_u32(
                &mut f.cells,
                OBJECT + f.view.field_layout.slab_handle_byte as usize,
                0,
            ),
            4 => set_u32(
                &mut f.cells,
                SLAB + f.view.field_layout.slab_capacity_byte as usize,
                2,
            ),
            5 => set_u8(
                &mut f.cells,
                SHAPE + f.view.shape_state_byte as usize,
                ShapeState::ORDINARY.bits(),
            ),
            6 => set_u8(
                &mut f.cells,
                SHAPE + f.view.shape_state_byte as usize,
                ShapeState::ORDINARY
                    .with_dictionary(true)
                    .with_provisional(true)
                    .bits(),
            ),
            7 => set_u8(
                &mut f.cells,
                SHAPE + f.view.shape_state_byte as usize,
                ShapeState::ORDINARY
                    .with_dictionary(true)
                    .with_lookup(otter_vm::object::LookupFact::HostLookup, true)
                    .bits(),
            ),
            8 => set_u32(
                &mut f.cells,
                OBJECT + f.view.object_exotic_handle_byte as usize,
                0,
            ),
            9 => set_u32(
                &mut f.cells,
                EXOTIC + f.view.exotic_dictionary_layout_byte as usize,
                LAYOUT ^ (1 << 31),
            ),
            10 => *f.realm ^= 1,
            11 => set_u64(
                &mut f.thread,
                VM_THREAD_ACTIVE_REALM_CELL_OFFSET as usize,
                0,
            ),
            12 => *f.global = 0,
            _ => unreachable!(),
        }
        let before = f.cells.clone();
        for temps in [[1, 8], [9, 2]] {
            let (result, observed, _) = f.run(proof, temps);
            assert_eq!(result, 0, "refusal={refusal}");
            assert_eq!(observed[0], CANARY, "refusal={refusal}");
            assert_eq!(f.cells, before);
        }
    }
    let mut f = Fixture::new(4, false);
    let proof = f.object(false, FieldLocation::inline(3));
    set_u8(
        &mut f.cells,
        SHAPE + f.view.shape_inline_capacity_byte as usize,
        3,
    );
    let before = f.cells.clone();
    assert_eq!(
        f.run(proof, [1, 8]).1[0],
        CANARY,
        "inline bounds refuse before result"
    );
    assert_eq!(f.cells, before);
}
