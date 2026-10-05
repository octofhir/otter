//! Executable probes of the one production x86-64 property-action emitter.
//!
//! # Contents
//! - Full-width keys, four-way collisions and independent load/store slots.
//! - Live capacities zero/four/sixty-four and replacement suffix handles.
//! - Guard refusal atomicity and preservation of all unassigned registers.
//!
//! # Invariants
//! - Owned aligned fixture words model the VM DTO's physical offsets; no raw
//!   fixture pointer crosses a VM allocation, C call or collector boundary.
//! - Each probe invokes the production shared emitter consumed by both tiers.
//! - These tests prove encoding and effects. Runtime moving-GC and cache root
//!   lifetime proofs remain the responsibility of the complete VM/native gate.
//!
//! # See also
//! - [`super::emit_action_probe`] for the production machine-code owner.

use dynasmrt::{AssemblyOffset, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, jit::JitPropertyActionCache, object::ShapeState, value::tag};

use super::emit_action_probe;
use crate::artifact::relocation::{PropertySourceAccess, RelocationCapture};

const ATOM: u32 = 0xfedc_ba98;
const RECEIVER_ID: u64 = 0x1_0000_0047;
const HOLDER_ID: u64 = 0x2_0000_005b;
const TARGET_ID: u64 = 0x3_0000_0070;
// Deliberately spaced fixture identity: the emitter must consume its DTO
// offset rather than duplicate the private ShapeBody layout. The canonical
// VM entry offsets below are pinned by its production compile-time assertions.
const SHAPE_ID_BYTE: u32 = 128;
const HOLDER: usize = 1024;
const SHAPE: usize = 2048;
const HOLDER_SHAPE: usize = 2304;
const TARGET: usize = 2560;
const ROOT: usize = 2816;
const SLAB: usize = 3072;
const HOLDER_SLAB: usize = 3328;
const REPLACEMENT_SLAB: usize = 3584;
const CANARY: u64 = 0x5555_5555_5555_5555;

fn snapshot() -> JitCompileSnapshot {
    let mut view = JitCompileSnapshot::without_feedback(
        943,
        3,
        3,
        vec![otter_vm::jit::JitTestInstruction::new(
            otter_bytecode::Op::ReturnUndefined,
            0,
            0,
            vec![],
        )],
    );
    // without_feedback intentionally carries no resident object/shape layout.
    // Give this stationary scalar model distinct offsets for every consumed
    // DTO field; the current FieldLayout still owns prefix/suffix storage.
    view.object_shape_byte = 8;
    view.shape_state_byte = 8;
    view.shape_inline_capacity_byte = 9;
    view.shape_prototype_byte = 12;
    assert_ne!(view.object_shape_byte, 0, "object tag cannot alias shape");
    assert_ne!(view.shape_state_byte, view.shape_inline_capacity_byte);
    assert!(view.shape_prototype_byte >= view.shape_inline_capacity_byte + 1);
    assert!(view.shape_prototype_byte + 4 <= SHAPE_ID_BYTE);
    view
}

struct Fixture {
    cells: Vec<u64>,
    table: Vec<u64>,
    validity: Box<u32>,
    view: JitCompileSnapshot,
    cache: JitPropertyActionCache,
    entry: usize,
    capacity: u8,
    atom: u32,
}

impl Fixture {
    fn new(capacity: u8) -> Self {
        let atom = if capacity == 4 { 0 } else { ATOM };
        let cells = vec![CANARY; 1024];
        let table = vec![0_u64; 512 * 4 * 8];
        let validity = Box::new(1);
        let cache = JitPropertyActionCache {
            table_addr: table.as_ptr() as usize,
            entry_bytes: 64,
            set_mask: 511,
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
        };
        let hash = RECEIVER_ID.wrapping_mul(cache.hash_shape_multiplier)
            ^ u64::from(atom).wrapping_mul(cache.hash_atom_multiplier);
        let set = ((hash >> cache.hash_shift) & u64::from(cache.set_mask)) as usize;
        let mut fixture = Self {
            cells,
            table,
            validity,
            view: snapshot(),
            cache,
            entry: (set * 4 + 3) * 64,
            capacity,
            atom,
        };
        let start = set * 4 * 64;
        for way in 0..3 {
            // Identical low 32 shape bits must not select these collisions.
            fixture.table_u64(start + way * 64, RECEIVER_ID ^ (1_u64 << 41));
            fixture.table_u32(start + way * 64 + 8, atom);
        }
        fixture.table_u64(fixture.entry, RECEIVER_ID);
        fixture.table_u32(fixture.entry + 8, atom);
        let object_shape = fixture.view.object_shape_byte as usize;
        let slab_handle = fixture.view.field_layout.slab_handle_byte as usize;
        for (object, shape, slab) in [(0, SHAPE, SLAB), (HOLDER, HOLDER_SHAPE, HOLDER_SLAB)] {
            fixture.cell_u8(object, crate::entry::OBJECT_BODY_TYPE_TAG as u8);
            fixture.cell_u32(object + object_shape, fixture.address(shape) as u32);
            fixture.cell_u32(object + slab_handle, fixture.address(slab) as u32);
        }
        for (shape, id) in [
            (SHAPE, RECEIVER_ID),
            (HOLDER_SHAPE, HOLDER_ID),
            (TARGET, TARGET_ID),
        ] {
            fixture.cell_u64(shape + SHAPE_ID_BYTE as usize, id);
            fixture.cell_u8(
                shape + fixture.view.shape_state_byte as usize,
                ShapeState::ORDINARY.bits(),
            );
            fixture.cell_u8(
                shape + fixture.view.shape_inline_capacity_byte as usize,
                capacity,
            );
            fixture.cell_u32(shape + fixture.view.shape_prototype_byte as usize, 0);
        }
        fixture.cell_u32(
            ROOT + fixture.view.shape_prototype_byte as usize,
            fixture.address(HOLDER) as u32,
        );
        for slab in [SLAB, HOLDER_SLAB, REPLACEMENT_SLAB] {
            fixture.cell_u32(
                slab + fixture.view.field_layout.slab_capacity_byte as usize,
                8,
            );
        }
        let upper = fixture.address(0) & 0xffff_ffff_0000_0000;
        assert_eq!(
            upper,
            fixture.address(fixture.cells.len() * 8 - 1) & 0xffff_ffff_0000_0000,
            "all compressed fixture handles belong to one cage segment"
        );
        fixture
    }

    fn address(&self, byte: usize) -> u64 {
        self.cells.as_ptr() as u64 + byte as u64
    }
    fn bank_byte(&self, object: usize, slab: usize, slot: u16) -> usize {
        if slot < u16::from(self.capacity) {
            object + self.view.field_layout.inline_values_byte as usize + usize::from(slot) * 8
        } else {
            slab + self.view.field_layout.slab_words_byte as usize
                + usize::from(slot - u16::from(self.capacity)) * 8
        }
    }
    fn cell_u8(&mut self, byte: usize, value: u8) {
        assert!(byte < self.cells.len() * 8);
        // SAFETY: this byte is inside owned aligned, stationary fixture words.
        unsafe { self.cells.as_mut_ptr().cast::<u8>().add(byte).write(value) };
    }
    fn cell_u32(&mut self, byte: usize, value: u32) {
        assert_eq!(byte % 4, 0);
        assert!(byte + 4 <= self.cells.len() * 8);
        // SAFETY: the range is aligned, initialized and inside owned words.
        unsafe {
            self.cells
                .as_mut_ptr()
                .cast::<u8>()
                .add(byte)
                .cast::<u32>()
                .write(value)
        };
    }
    fn cell_u64(&mut self, byte: usize, value: u64) {
        assert_eq!(byte % 8, 0);
        self.cells[byte / 8] = value;
    }
    fn table_u8(&mut self, byte: usize, value: u8) {
        assert!(byte < self.table.len() * 8);
        // SAFETY: this byte is inside owned aligned, stationary table words.
        unsafe { self.table.as_mut_ptr().cast::<u8>().add(byte).write(value) };
    }
    fn table_u16(&mut self, byte: usize, value: u16) {
        assert_eq!(byte % 2, 0);
        assert!(byte + 2 <= self.table.len() * 8);
        // SAFETY: this range is aligned and inside owned table words.
        unsafe {
            self.table
                .as_mut_ptr()
                .cast::<u8>()
                .add(byte)
                .cast::<u16>()
                .write(value)
        };
    }
    fn table_u32(&mut self, byte: usize, value: u32) {
        assert_eq!(byte % 4, 0);
        assert!(byte + 4 <= self.table.len() * 8);
        // SAFETY: this range is aligned and inside owned table words.
        unsafe {
            self.table
                .as_mut_ptr()
                .cast::<u8>()
                .add(byte)
                .cast::<u32>()
                .write(value)
        };
    }
    fn table_u64(&mut self, byte: usize, value: u64) {
        assert_eq!(byte % 8, 0);
        self.table[byte / 8] = value;
    }
    fn own_load(&mut self, slot: u16) {
        self.table_u8(
            self.entry + 12,
            otter_vm::jit::PropertyLoadAction::OwnData as u8,
        );
        self.table_u16(self.entry + 14, slot);
        self.table_u32(self.entry + 20, self.address(SHAPE) as u32);
        self.table_u64(self.entry + 32, RECEIVER_ID);
    }
    fn inherited_load_and_append(&mut self, load_slot: u16, store_slot: u16) {
        self.table_u8(
            self.entry + 12,
            otter_vm::jit::PropertyLoadAction::InheritedData as u8,
        );
        self.table_u16(self.entry + 14, load_slot);
        self.table_u8(self.entry + 18, 1);
        self.table_u32(self.entry + 20, self.address(HOLDER_SHAPE) as u32);
        self.table_u32(self.entry + 24, self.address(ROOT) as u32);
        self.table_u64(self.entry + 32, HOLDER_ID);
        self.table_u64(self.entry + 48, self.validity.as_ref() as *const u32 as u64);
        self.cell_u32(
            SHAPE + self.view.shape_prototype_byte as usize,
            self.address(HOLDER) as u32,
        );
        self.cell_u32(
            TARGET + self.view.shape_prototype_byte as usize,
            self.address(HOLDER) as u32,
        );
        self.append(store_slot, true);
    }
    fn append(&mut self, slot: u16, chain: bool) {
        self.table_u8(
            self.entry + 13,
            otter_vm::jit::PropertyStoreAction::AddOwn as u8,
        );
        self.table_u16(self.entry + 16, slot);
        self.table_u32(self.entry + 28, self.address(TARGET) as u32);
        self.table_u64(self.entry + 40, TARGET_ID);
        self.table_u64(
            self.entry + 56,
            if chain {
                self.validity.as_ref() as *const u32 as u64
            } else {
                0
            },
        );
    }
    fn run(&mut self, access: PropertySourceAccess, atom: u32, input: u64) -> (u64, [u64; 7]) {
        let mut ops = Assembler::new().unwrap();
        let miss = ops.new_dynamic_label();
        let done = ops.new_dynamic_label();
        let appended = ops.new_dynamic_label();
        let exit = ops.new_dynamic_label();
        dynasm!(ops ; .arch x64
            ; push rbx ; push r12 ; push r13 ; push r14 ; push r15
            ; mov r12, rdx
            ; mov rbx, QWORD 0x1111_1111_1111_1111_u64 as i64
            ; mov r13, QWORD 0x2222_2222_2222_2222_u64 as i64
            ; mov r14, QWORD 0x3333_3333_3333_3333_u64 as i64
            ; mov r15, QWORD 0x4444_4444_4444_4444_u64 as i64);
        let load = matches!(access, PropertySourceAccess::Load);
        emit_action_probe(
            &mut ops,
            &mut RelocationCapture::new(false),
            &self.view,
            Some(self.cache),
            Some(super::AtomOperand::Immediate(atom)),
            access,
            7,
            (!load).then_some(6),
            [1, 2, 8, 9],
            load.then_some(0),
            miss,
            done,
            appended,
        );
        dynasm!(ops ; .arch x64
            ; =>miss ; xor eax, eax ; jmp =>exit
            ; =>appended ; mov [r12 + 48], rdx ; mov eax, 2 ; jmp =>exit
            ; =>done);
        if !load {
            dynasm!(ops ; .arch x64 ; mov eax, 1);
        }
        dynasm!(ops ; .arch x64
            ; =>exit
            ; mov [r12], rdi ; mov [r12 + 8], rsi ; mov [r12 + 16], rbx
            ; mov [r12 + 24], r13 ; mov [r12 + 32], r14 ; mov [r12 + 40], r15
            ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; pop rbx ; ret);
        let buffer = ops.finalize().unwrap();
        // SAFETY: the generated SysV probe accesses only the stationary owned
        // fixture words/table and retained Box validity word, and saves every
        // nonvolatile register it uses. It makes no runtime or collector call.
        let probe: unsafe extern "sysv64" fn(u64, u64, *mut u64) -> u64 =
            unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
        let mut observed = [0_u64; 7];
        // SAFETY: the fixture buffers stay owned and stationary for this
        // complete no-call probe; the output array has seven writable words.
        let result = unsafe { probe(self.address(0), input, observed.as_mut_ptr()) };
        assert_eq!(
            observed[..6],
            [
                self.address(0),
                input,
                0x1111_1111_1111_1111,
                0x2222_2222_2222_2222,
                0x3333_3333_3333_3333,
                0x4444_4444_4444_4444
            ]
        );
        (result, observed)
    }
}

#[test]
fn one_key_keeps_independent_inherited_load_and_append_slots() {
    for capacity in [0, 4, 64] {
        let mut f = Fixture::new(capacity);
        let load_slot = 0;
        let store_slot = u16::from(capacity) + 1;
        f.inherited_load_and_append(load_slot, store_slot);
        let held = tag::NUMBER_TAG | 771;
        let stored = tag::NUMBER_TAG | 991;
        let load_byte = f.bank_byte(HOLDER, HOLDER_SLAB, load_slot);
        let store_byte = f.bank_byte(0, SLAB, store_slot);
        f.cell_u64(load_byte, held);
        let before = f.cells.clone();
        assert_eq!(f.run(PropertySourceAccess::Load, f.atom, stored).0, held);
        assert_eq!(f.cells, before, "load preserves every fixture word");
        f.table_u8(
            f.entry + 13,
            otter_vm::jit::PropertyStoreAction::Unknown as u8,
        );
        assert_eq!(f.run(PropertySourceAccess::Store, f.atom, stored).0, 0);
        assert_eq!(
            f.cells, before,
            "inherited writable load alone does not authorize append"
        );
        f.append(store_slot, true);
        let (outcome, regs) = f.run(PropertySourceAccess::Store, f.atom, stored);
        assert_eq!(outcome, 2);
        assert_eq!(regs[6], u64::from(f.address(TARGET) as u32));
        let mut expected = before;
        expected[store_byte / 8] = stored;
        let shape_byte = f.view.object_shape_byte as usize;
        let shift = (shape_byte % 8) * 8;
        expected[shape_byte / 8] = (expected[shape_byte / 8] & !(0xffff_ffff_u64 << shift))
            | (u64::from(f.address(TARGET) as u32) << shift);
        assert_eq!(
            f.cells, expected,
            "only receiver append slot and shape are written"
        );
        assert_eq!(
            f.cells[load_byte / 8],
            held,
            "inherited holder slot remains independent"
        );
    }
}

#[test]
fn own_data_reads_readonly_and_current_suffix_while_store_requires_its_action() {
    for capacity in [0, 4, 64] {
        let mut f = Fixture::new(capacity);
        let slot = u16::from(capacity) + 2;
        f.own_load(slot);
        let first = f.bank_byte(0, SLAB, slot);
        let second = f.bank_byte(0, REPLACEMENT_SLAB, slot);
        let value = tag::NUMBER_TAG | 331;
        f.cell_u64(first, value);
        f.cell_u64(second, value + 1);
        assert_eq!(f.run(PropertySourceAccess::Load, f.atom, 0).0, value);
        let before = f.cells.clone();
        assert_eq!(f.run(PropertySourceAccess::Store, f.atom, value + 2).0, 0);
        assert_eq!(
            f.cells, before,
            "read-only load never implies writable store"
        );
        f.cell_u32(
            f.view.field_layout.slab_handle_byte as usize,
            f.address(REPLACEMENT_SLAB) as u32,
        );
        assert_eq!(f.run(PropertySourceAccess::Load, f.atom, 0).0, value + 1);
        f.table_u8(
            f.entry + 13,
            otter_vm::jit::PropertyStoreAction::OwnWritable as u8,
        );
        f.table_u16(f.entry + 16, slot);
        f.table_u8(f.entry + 18, 1);
        assert_eq!(f.run(PropertySourceAccess::Store, f.atom, value + 2).0, 1);
        assert_eq!(f.cells[second / 8], value + 2);
        assert_eq!(
            f.cells[first / 8],
            value,
            "retired suffix storage is untouched"
        );
        // Non-extensible shapes still permit a proven existing writable slot.
        f.cell_u8(
            SHAPE + f.view.shape_state_byte as usize,
            ShapeState::ORDINARY.with_extensible(false).bits(),
        );
        assert_eq!(f.run(PropertySourceAccess::Store, f.atom, value + 3).0, 1);
        let before_inline = f.cells.clone();
        f.table_u16(f.entry + 16, 0);
        assert_eq!(f.run(PropertySourceAccess::Store, f.atom, value + 4).0, 1);
        let mut expected = before_inline;
        expected[f.bank_byte(0, REPLACEMENT_SLAB, 0) / 8] = value + 4;
        assert_eq!(
            f.cells, expected,
            "existing overwrite selects the current prefix or suffix"
        );
    }
}

#[test]
fn every_miss_finishes_before_any_slot_or_shape_effect() {
    for refusal in 0..13 {
        let mut f = Fixture::new(4);
        f.append(5, false);
        match refusal {
            0 => f.table_u64(f.entry, RECEIVER_ID ^ (1_u64 << 41)),
            1 => f.table_u32(f.entry + 8, f.atom ^ (1_u32 << 31)),
            2 => f.table_u8(
                f.entry + 13,
                otter_vm::jit::PropertyStoreAction::Unknown as u8,
            ),
            3 => f.cell_u8(
                SHAPE + f.view.shape_state_byte as usize,
                ShapeState::ORDINARY.with_dictionary(true).bits(),
            ),
            4 => f.cell_u8(
                SHAPE + f.view.shape_state_byte as usize,
                ShapeState::ORDINARY
                    .with_lookup(otter_vm::object::LookupFact::HostLookup, true)
                    .bits(),
            ),
            5 => f.cell_u8(
                SHAPE + f.view.shape_state_byte as usize,
                ShapeState::ORDINARY.with_prototype_role(true).bits(),
            ),
            6 => f.cell_u8(
                SHAPE + f.view.shape_state_byte as usize,
                ShapeState::ORDINARY.with_extensible(false).bits(),
            ),
            7 => {
                f.table_u64(f.entry + 56, f.validity.as_ref() as *const u32 as u64);
                *f.validity = 0;
            }
            8 => f.table_u32(f.entry + 28, 0),
            9 => f.table_u64(f.entry + 40, TARGET_ID ^ (1_u64 << 32)),
            10 => f.cell_u32(f.view.field_layout.slab_handle_byte as usize, 0),
            11 => f.cell_u32(SLAB + f.view.field_layout.slab_capacity_byte as usize, 1),
            12 => f.cell_u8(0, 0xff),
            _ => unreachable!(),
        }
        let before = f.cells.clone();
        assert_eq!(
            f.run(PropertySourceAccess::Store, f.atom, tag::NUMBER_TAG | 118)
                .0,
            0,
            "refusal {refusal}"
        );
        assert_eq!(
            f.cells, before,
            "refusal {refusal} must not publish any effect"
        );
    }
    for refusal in 0..6 {
        let mut f = Fixture::new(4);
        f.inherited_load_and_append(0, 5);
        match refusal {
            0 => *f.validity = 0,
            1 => f.table_u32(f.entry + 24, 0),
            2 => f.table_u32(f.entry + 20, f.address(SHAPE) as u32),
            3 => f.table_u64(f.entry + 32, HOLDER_ID ^ (1_u64 << 32)),
            4 => f.table_u8(
                f.entry + 12,
                otter_vm::jit::PropertyLoadAction::Unresolvable as u8,
            ),
            5 => f.table_u64(f.entry + 48, 0),
            _ => unreachable!(),
        }
        let before = f.cells.clone();
        assert_eq!(
            f.run(PropertySourceAccess::Load, f.atom, 0).0,
            0,
            "load refusal {refusal}"
        );
        assert_eq!(f.cells, before);
    }
    let mut f = Fixture::new(4);
    f.append(5, false);
    f.cell_u8(
        SHAPE + f.view.shape_state_byte as usize,
        ShapeState::ORDINARY.with_provisional(true).bits(),
    );
    assert_eq!(
        f.run(PropertySourceAccess::Store, f.atom, tag::NUMBER_TAG | 818)
            .0,
        2,
        "dynamic provisional ordinary feedback remains eligible"
    );
}
