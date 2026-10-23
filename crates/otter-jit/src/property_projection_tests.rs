//! Stationary storage geometry shared by native Array-expando IC tests.
//!
//! # Contents
//! - A live nullable Array edge, two own storage objects and one current IC row.
//! - Source-preservation and storage-owner captures from the production emitter.
//! - Valid value stores remain legal with a zero observation owner.
//!
//! # Invariants
//! - Initialized aligned host words model only the fields each emitter reads.
//! - The current VM IC DTO owns every row offset; no fixture duplicates it.
//! - A zero observation owner excludes every helper and collector boundary.
//!
//! # See also
//! - `arm64::property_ic` and `x86_64::property_ic` consume this geometry.

use otter_vm::JitCompileSnapshot;
use otter_vm::jit::{
    JitArrayLayout, PROPERTY_IC_LAYOUT, PropertyIcHandlerKind, PropertyStorageProjection,
};

const BAG_A: usize = 64;
const BAG_B: usize = 96;
const SHAPE: usize = 128;
const SLOT: usize = 256;
pub(super) const FIELD: usize = 16;
pub(super) const ORIGINAL: u64 = 0xaaaa_bbbb_cccc_dddd;

#[repr(C)]
#[derive(Default)]
pub(super) struct Invocation {
    pub(super) receiver: usize,
    pub(super) value: u64,
    pub(super) slot: usize,
    pub(super) returned_receiver: usize,
    pub(super) returned_slot: usize,
    pub(super) child: u64,
    pub(super) returned_value: u64,
}

pub(super) struct Fixture {
    cells: Box<[u64; 128]>,
    pub(super) view: JitCompileSnapshot,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let mut view = JitCompileSnapshot::without_feedback(1, 0, 0, vec![]);
        view.native_call_layout = otter_vm::jit::JitNativeCallLayout::current();
        view.array_layout = JitArrayLayout {
            type_tag: otter_vm::array::ARRAY_BODY_TYPE_TAG,
            length_byte: 16,
            expando_byte: 8,
        };
        view.object_shape_byte = 8;
        view.field_layout.inline_values_byte = FIELD as u32;
        let mut fixture = Self {
            cells: Box::new([0; 128]),
            view,
        };
        assert_eq!(fixture.array() >> 32, fixture.address(SLOT) >> 32);
        let ic = PROPERTY_IC_LAYOUT;
        fixture.write(0, otter_vm::array::ARRAY_BODY_TYPE_TAG);
        fixture.set_edge(Some(false));
        for bag in [BAG_A, BAG_B] {
            fixture.write(bag, crate::entry::OBJECT_BODY_TYPE_TAG as u8);
            fixture.write(
                bag + fixture.view.object_shape_byte as usize,
                fixture.address(SHAPE) as u32,
            );
            fixture.write(bag + FIELD, ORIGINAL);
        }
        fixture.write(SLOT + ic.state_byte as usize, 1u32);
        fixture.write(
            SLOT + ic.entries_byte as usize + ic.entry_shape_byte as usize,
            PropertyStorageProjection::ArrayExpando.encode_key(fixture.address(SHAPE) as u32),
        );
        fixture.write(
            SLOT + ic.entries_byte as usize + ic.entry_field_byte as usize,
            otter_vm::object::FieldLocation::inline(0).cache_key(),
        );
        fixture.set_kind(PropertyIcHandlerKind::StoreField);
        fixture
    }

    fn address(&self, byte: usize) -> usize {
        self.cells.as_ptr() as usize + byte
    }

    fn write<T: Copy>(&mut self, byte: usize, value: T) {
        assert!(byte + std::mem::size_of::<T>() <= std::mem::size_of_val(&*self.cells));
        // SAFETY: the initialized host allocation bounds the entire write.
        unsafe {
            (self.cells.as_mut_ptr().cast::<u8>().add(byte).cast::<T>()).write_unaligned(value)
        };
    }

    pub(super) fn array(&self) -> usize {
        self.address(0)
    }
    pub(super) fn bag(&self, second: bool) -> usize {
        self.address(if second { BAG_B } else { BAG_A })
    }
    pub(super) fn set_edge(&mut self, second: Option<bool>) {
        let edge = second.map_or(0, |second| self.bag(second) as u32);
        self.write(self.view.array_layout.expando_byte as usize, edge);
    }
    pub(super) fn set_kind(&mut self, kind: PropertyIcHandlerKind) {
        let ic = PROPERTY_IC_LAYOUT;
        self.write(
            SLOT + ic.entries_byte as usize + ic.entry_kind_byte as usize,
            kind as u8,
        );
    }
    pub(super) fn set_field(&mut self, physical: u64) {
        self.write(BAG_A + FIELD, physical);
    }
    pub(super) fn field(&self, second: bool) -> u64 {
        self.cells[((if second { BAG_B } else { BAG_A }) + FIELD) / 8]
    }
    pub(super) fn invocation(&self) -> Invocation {
        Invocation {
            receiver: self.array(),
            value: crate::entry::VALUE_TRUE,
            slot: self.address(SLOT),
            ..Default::default()
        }
    }
}

pub(super) fn check_store_contract(mut call: impl FnMut(&mut Invocation) -> usize) {
    use otter_vm::Value;
    for value in [
        Value::number_i32(7),
        Value::number_f64(1.25),
        Value::UNDEFINED,
        Value::TRUE,
    ] {
        let fixture = Fixture::new();
        let mut invocation = fixture.invocation();
        invocation.value = value.to_bits();
        assert_eq!(call(&mut invocation), 1);
        assert_eq!(invocation.returned_receiver, invocation.receiver);
        assert_eq!(invocation.returned_value, invocation.value);
        assert_eq!(fixture.field(false), invocation.value);
        assert_eq!(fixture.field(true), ORIGINAL);
    }
    let mut fixture = Fixture::new();
    let mut invocation = fixture.invocation();
    assert_eq!(call(&mut invocation), 1);
    assert_eq!(invocation.returned_value, invocation.value);
    assert_eq!(invocation.returned_receiver, fixture.array());
    assert_eq!(
        invocation.returned_slot,
        fixture.bag(false),
        "the barrier parent is storage, not the array"
    );
    assert_eq!(invocation.child, 0);
    assert_eq!(fixture.field(false), invocation.value);
    assert_eq!(fixture.field(true), ORIGINAL);
    fixture.set_edge(Some(true));
    assert_eq!(call(&mut invocation), 1);
    assert_eq!(
        invocation.returned_slot,
        fixture.bag(true),
        "the current compressed edge selects the owner"
    );
    assert_eq!(fixture.field(true), invocation.value);

    for kind in [
        PropertyIcHandlerKind::StoreTransition,
        PropertyIcHandlerKind::OwnField,
    ] {
        let mut fixture = Fixture::new();
        fixture.set_kind(kind);
        let mut invocation = fixture.invocation();
        assert_eq!(call(&mut invocation), 0);
        assert_eq!(invocation.returned_receiver, invocation.receiver);
        assert_eq!(invocation.returned_slot, invocation.slot);
        assert_eq!(
            fixture.field(false),
            ORIGINAL,
            "no projected append or read commits a store"
        );
    }
    fixture.set_edge(None);
    invocation.returned_slot = 0;
    assert_eq!(call(&mut invocation), 0);
    assert_eq!(invocation.returned_receiver, invocation.receiver);
    assert_eq!(invocation.returned_slot, invocation.slot);
}

pub(super) fn check_double_load_contract(mut call: impl FnMut(&mut Invocation) -> u64) {
    use otter_vm::Value;
    let mut fixture = Fixture::new();
    fixture.set_kind(PropertyIcHandlerKind::OwnField);
    for value in [
        Value::number_i32(7),
        Value::number_f64(-0.0),
        Value::number_f64(1.25),
        Value::number_f64(f64::NAN),
        Value::UNDEFINED,
    ] {
        let physical = if value.is_number() {
            otter_vm::Value::double(value.as_f64().unwrap())
        } else {
            value
        };
        fixture.set_field(physical.to_bits());
        let mut invocation = fixture.invocation();
        assert_eq!(call(&mut invocation), physical.to_bits());
    }
}
