//! Per-site named-property inline caches: V8's feedback slot and handlers.
//!
//! Each `LoadProperty`, `StoreProperty` and the method lookup of
//! `CallMethodValue` owns one [`PropertyIcSlot`] in its CodeBlock's feedback
//! vector. The slot is mutable data that the VM and generated code both read:
//! the receiver's shape selects an [`IcEntry`] whose handler says how to finish
//! the access. Misses update the slot through V8's state machine
//! (`IC::SetCache`); generated code never changes when the feedback does.
//!
//! # Contents
//! - [`PropertyIcSlot`] — the native slot: an inline own-field pair for the
//!   generated fast path, the site state and key, and up to
//!   [`PROFILED_PROPERTY_PIC_CAPACITY`] `(shape, handler)` entries.
//! - [`IcHandler`] / [`IcHandlerKind`] — what to do once a shape matched:
//!   own field, prototype field, nonexistent, own-field store, transition.
//! - [`PropertyIcStats`] — aggregate counters for diagnostics and tests.
//!
//! # Invariants
//! - Entries name shapes by compressed handle, weakly. The full collector's
//!   weak pass removes every entry whose receiver shape, holder root or
//!   transition target died before those cells can be reused, so a handle
//!   compare is an exact layout proof. Only fast ordinary shapes are
//!   installed; dictionary and opaque objects never match an entry.
//! - A receiver whose named lookup starts at another object is keyed by that
//!   lookup-start object's shape with the low bit set (shape handles are
//!   8-aligned, so the key never equals an ordinary receiver's shape), and
//!   its handlers act on the lookup-start object (V8's
//!   `lookup_start_object`): a function's property bag, which carries the
//!   function's own `[[Prototype]]`, for every key the function does not
//!   synthesize; an ordinary dense array's prototype, for every named key
//!   but `length`.
//! - A matched receiver shape fixes the object's lookup state, own keys,
//!   attributes, prototype and storage banks. Prototype-field and
//!   nonexistent handlers additionally require their chain proof; a
//!   transition requires its proof (or a `null` prototype, which the receiver
//!   shape fixes). An invalid proof only misses.
//! - Store handlers are never installed for prototype-role receivers: their
//!   mutations must reach the invalidation boundary.
//! - One VM thread writes a slot; generated code reads it on that thread.
//! - Megamorphic is terminal; its receivers use the isolate's shared action
//!   table, which every resolution already records.
//! - An entry owns one strong count of its chain proof (`validity` word).
//!
//! # See also
//! - [`crate::cache_ir`] — the resolutions handlers cache.
//! - [`crate::property_cache`] — the shared `(shape, atom)` table.
//! - `otter_jit` Template property sites — the generated readers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::Value;
use crate::object::prototype_validity::PrototypeValidity;
use crate::object::{self, FieldLocation, JsObject, ShapeHandle, StorePropertyTransitionKind};
use crate::property_atom::AtomizedPropertyKey;
use crate::tier_policy::PROFILED_PROPERTY_PIC_CAPACITY;

/// Aggregate inline-cache counters for named property loads and stores.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PropertyIcStats {
    /// `LoadProperty` handler hits served by the VM.
    pub load_hits: u64,
    /// `LoadProperty` object receivers that missed or had no IC entry.
    pub load_misses: u64,
    /// `LoadProperty` IC entries installed or replaced.
    pub load_installs: u64,
    /// `LoadProperty` sites that became megamorphic.
    pub load_disables: u64,
    /// `StoreProperty` handler hits served by the VM.
    pub store_hits: u64,
    /// `StoreProperty` ordinary object receivers that missed or had no entry.
    pub store_misses: u64,
    /// `StoreProperty` IC entries installed or replaced.
    pub store_installs: u64,
    /// `StoreProperty` sites that became megamorphic.
    pub store_disables: u64,
}

impl PropertyIcStats {
    /// Add another counter set.
    pub(crate) fn add(&mut self, other: Self) {
        self.load_hits = self.load_hits.saturating_add(other.load_hits);
        self.load_misses = self.load_misses.saturating_add(other.load_misses);
        self.load_installs = self.load_installs.saturating_add(other.load_installs);
        self.load_disables = self.load_disables.saturating_add(other.load_disables);
        self.store_hits = self.store_hits.saturating_add(other.store_hits);
        self.store_misses = self.store_misses.saturating_add(other.store_misses);
        self.store_installs = self.store_installs.saturating_add(other.store_installs);
        self.store_disables = self.store_disables.saturating_add(other.store_disables);
    }
}

/// Property opcode family for shared IC lifecycle accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyIcKind {
    /// `LoadProperty` site, or the method lookup of `CallMethodValue`.
    Load,
    /// `StoreProperty` site.
    Store,
}

/// What a matched entry does: V8's `LoadHandler::Kind` / `StoreHandler::Kind`
/// subset for Otter's ordinary objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IcHandlerKind {
    /// Unused entry.
    Empty = 0,
    /// Load the receiver's own data field.
    OwnField = 1,
    /// Load a data field of the holder `prototype(aux)`, authorized by the
    /// entry's chain proof.
    PrototypeField = 2,
    /// The key is absent from the receiver and its chain: `undefined`.
    NonExistent = 3,
    /// Store into the receiver's existing writable own data field.
    StoreField = 4,
    /// Append the field and publish the target shape `aux`.
    StoreTransition = 5,
}

impl IcHandlerKind {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::OwnField,
            2 => Self::PrototypeField,
            3 => Self::NonExistent,
            4 => Self::StoreField,
            5 => Self::StoreTransition,
            _ => Self::Empty,
        }
    }

    const fn is_store(self) -> bool {
        matches!(self, Self::StoreField | Self::StoreTransition)
    }
}

/// The prototype guard of a [`IcHandlerKind::StoreTransition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransitionGuard {
    /// The receiver shape fixes a `null` prototype.
    OwnAdd = 0,
    /// No link of the proved chain owns the key.
    ChainMissing = 1,
    /// The direct prototype owns writable data of the key.
    PrototypeWritable = 2,
}

/// The empty shape word. Shape handles are 8-aligned non-zero cage offsets,
/// so this never equals a receiver's shape and the inline compare needs no
/// separate emptiness test.
pub const EMPTY_SHAPE: u32 = u32::MAX;

/// Key bit of a receiver whose named lookup starts at another object: that
/// lookup-start object's shape handle with the low bit set.
pub const LOOKUP_START_KEY_BIT: u32 = 1;

const STATE_COUNT_MASK: u32 = 0xff;
const STATE_ATTEMPTED: u32 = 1 << 8;
const STATE_MEGAMORPHIC: u32 = 1 << 9;

/// One `(receiver shape, handler)` pair, read by generated code.
#[derive(Debug)]
#[repr(C)]
pub struct IcEntry {
    shape: AtomicU32,
    field: AtomicU32,
    kind: AtomicU8,
    transition: AtomicU8,
    slot: AtomicU16,
    aux: AtomicU32,
    validity: AtomicU64,
}

impl IcEntry {
    const fn new() -> Self {
        Self {
            shape: AtomicU32::new(EMPTY_SHAPE),
            field: AtomicU32::new(0),
            kind: AtomicU8::new(IcHandlerKind::Empty as u8),
            transition: AtomicU8::new(0),
            slot: AtomicU16::new(0),
            aux: AtomicU32::new(0),
            validity: AtomicU64::new(0),
        }
    }

    fn shape(&self) -> u32 {
        self.shape.load(Ordering::Relaxed)
    }

    fn kind(&self) -> IcHandlerKind {
        IcHandlerKind::from_raw(self.kind.load(Ordering::Relaxed))
    }

    fn field(&self) -> FieldLocation {
        FieldLocation::from_cache_key(self.field.load(Ordering::Relaxed))
    }

    fn slot(&self) -> u16 {
        self.slot.load(Ordering::Relaxed)
    }

    fn aux(&self) -> ShapeHandle {
        // SAFETY: `aux` only ever holds a shape handle (or zero) published
        // by `write`; the weak pass clears it with its entry.
        unsafe { ShapeHandle::from_offset(self.aux.load(Ordering::Relaxed)) }
    }

    fn validity_word(&self) -> u64 {
        self.validity.load(Ordering::Relaxed)
    }

    /// The entry's proof, if any, still holds.
    fn proof_holds(&self) -> bool {
        let word = self.validity_word();
        // SAFETY: a non-zero word owns a strong count (`write`).
        word == 0 || unsafe { PrototypeValidity::borrow_raw(word) }.is_valid()
    }

    fn write(&self, handler: IcHandler) {
        let key = handler.key();
        let old = self.validity.swap(
            handler.validity.map_or(0, PrototypeValidity::into_raw),
            Ordering::Relaxed,
        );
        if old != 0 {
            // SAFETY: the previous word owned its count.
            unsafe { PrototypeValidity::release_raw(old) };
        }
        self.field
            .store(handler.field.cache_key(), Ordering::Relaxed);
        self.kind.store(handler.kind as u8, Ordering::Relaxed);
        self.transition
            .store(handler.transition as u8, Ordering::Relaxed);
        self.slot.store(handler.slot, Ordering::Relaxed);
        self.aux.store(handler.aux.offset(), Ordering::Relaxed);
        self.shape.store(key, Ordering::Relaxed);
    }

    /// Shape handle the entry's key names, with the function bit cleared.
    fn shape_handle(&self) -> ShapeHandle {
        // SAFETY: entry keys hold shape handles (plus the function bit)
        // published by `write`; the weak pass clears dead ones.
        unsafe { ShapeHandle::from_offset(self.shape() & !LOOKUP_START_KEY_BIT) }
    }

    fn lookup_start(&self) -> bool {
        self.shape() != EMPTY_SHAPE && self.shape() & LOOKUP_START_KEY_BIT != 0
    }

    /// Move `other`'s contents (including its proof count) into `self`.
    fn take_from(&self, other: &Self) {
        debug_assert_eq!(self.validity_word(), 0);
        self.shape.store(other.shape(), Ordering::Relaxed);
        self.field
            .store(other.field.load(Ordering::Relaxed), Ordering::Relaxed);
        self.kind
            .store(other.kind.load(Ordering::Relaxed), Ordering::Relaxed);
        self.transition
            .store(other.transition.load(Ordering::Relaxed), Ordering::Relaxed);
        self.slot.store(other.slot(), Ordering::Relaxed);
        self.aux
            .store(other.aux.load(Ordering::Relaxed), Ordering::Relaxed);
        self.validity
            .store(other.validity.swap(0, Ordering::Relaxed), Ordering::Relaxed);
        other.reset();
    }

    fn clear(&self) {
        let old = self.validity.swap(0, Ordering::Relaxed);
        if old != 0 {
            // SAFETY: the word owned its count.
            unsafe { PrototypeValidity::release_raw(old) };
        }
        self.reset();
    }

    fn reset(&self) {
        self.shape.store(EMPTY_SHAPE, Ordering::Relaxed);
        self.field.store(0, Ordering::Relaxed);
        self.kind
            .store(IcHandlerKind::Empty as u8, Ordering::Relaxed);
        self.transition.store(0, Ordering::Relaxed);
        self.slot.store(0, Ordering::Relaxed);
        self.aux.store(0, Ordering::Relaxed);
    }

    fn transition_guard(&self) -> TransitionGuard {
        match self.transition.load(Ordering::Relaxed) {
            1 => TransitionGuard::ChainMissing,
            2 => TransitionGuard::PrototypeWritable,
            _ => TransitionGuard::OwnAdd,
        }
    }
}

/// A handler ready to install: what an [`IcEntry`] holds, owned by Rust.
#[derive(Debug, Clone)]
pub(crate) struct IcHandler {
    receiver_shape: ShapeHandle,
    lookup_start: bool,
    kind: IcHandlerKind,
    field: FieldLocation,
    slot: u16,
    aux: ShapeHandle,
    validity: Option<Arc<PrototypeValidity>>,
    transition: TransitionGuard,
}

impl IcHandler {
    fn eligible_receiver(shape: ShapeHandle) -> bool {
        if shape.is_null() {
            return false;
        }
        let state = object::shape_body::state_of(shape);
        !state.is_dictionary() && !state.is_opaque()
    }

    /// The load handler for a resolved own or inherited data slot of a
    /// receiver whose layout is `receiver_shape`.
    #[must_use]
    pub(crate) fn load_resolved(
        receiver_shape: ShapeHandle,
        resolved: &crate::cache_ir::ResolvedDataSlot,
    ) -> Option<Self> {
        if !Self::eligible_receiver(receiver_shape) || resolved.hit.shape.is_null() {
            return None;
        }
        let field = object::field_location(resolved.hit.shape, u32::from(resolved.hit.slot));
        if resolved.hops == 0 {
            if resolved.hit.shape != receiver_shape {
                return None;
            }
            return Some(Self {
                receiver_shape,
                lookup_start: false,
                kind: IcHandlerKind::OwnField,
                field,
                slot: resolved.hit.slot,
                aux: ShapeHandle::null(),
                validity: None,
                transition: TransitionGuard::OwnAdd,
            });
        }
        if resolved.holder_root.is_null() {
            return None;
        }
        Some(Self {
            receiver_shape,
            lookup_start: false,
            kind: IcHandlerKind::PrototypeField,
            field,
            slot: resolved.hit.slot,
            aux: resolved.holder_root,
            validity: Some(resolved.validity.clone()?),
            transition: TransitionGuard::OwnAdd,
        })
    }

    /// The load handler for a key the receiver and its chain lack.
    #[must_use]
    pub(crate) fn load_nonexistent(
        receiver_shape: ShapeHandle,
        validity: Option<Arc<PrototypeValidity>>,
    ) -> Option<Self> {
        Self::eligible_receiver(receiver_shape).then(|| Self {
            receiver_shape,
            lookup_start: false,
            kind: IcHandlerKind::NonExistent,
            field: FieldLocation::from_cache_key(0),
            slot: 0,
            aux: ShapeHandle::null(),
            validity,
            transition: TransitionGuard::OwnAdd,
        })
    }

    /// This load handler, serving a receiver whose lookup-start object has the
    /// handler's receiver shape: a function's property bag or an ordinary
    /// dense array's prototype.
    #[must_use]
    pub(crate) fn for_lookup_start(mut self) -> Self {
        debug_assert!(
            !self.kind.is_store(),
            "lookup-start receivers take loads only"
        );
        self.lookup_start = true;
        self
    }

    fn key(&self) -> u32 {
        self.receiver_shape.offset()
            | if self.lookup_start {
                LOOKUP_START_KEY_BIT
            } else {
                0
            }
    }

    /// The store handler for the receiver's existing writable own data slot.
    #[must_use]
    pub(crate) fn store_existing(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<Self> {
        Self::store_existing_hit(obj, heap, key).map(|(handler, _)| handler)
    }

    /// [`Self::store_existing`] plus the own-property hit the shared action
    /// table records for the same receiver class.
    #[must_use]
    pub(crate) fn store_existing_hit(
        obj: JsObject,
        heap: &otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<(Self, object::AtomOwnPropertyHit)> {
        let receiver_shape = object::keyed_shape(obj, heap);
        if !Self::eligible_receiver(receiver_shape)
            || object::shape_body::state_of(receiver_shape).is_prototype()
        {
            return None;
        }
        let lookup = object::lookup_own_atom(obj, heap, key);
        let (Some(hit), object::PropertyLookup::Data { flags, .. }) = (lookup.hit, lookup.lookup)
        else {
            return None;
        };
        (flags.writable() && hit.shape == receiver_shape).then(|| {
            (
                Self {
                    receiver_shape,
                    lookup_start: false,
                    kind: IcHandlerKind::StoreField,
                    field: object::field_location(receiver_shape, u32::from(hit.slot)),
                    slot: hit.slot,
                    aux: ShapeHandle::null(),
                    validity: None,
                    transition: TransitionGuard::OwnAdd,
                },
                hit,
            )
        })
    }

    /// The store handler replaying a captured append from `from_shape`.
    #[must_use]
    pub(crate) fn store_transition(
        from_shape: ShapeHandle,
        transition: &object::StorePropertyTransition,
    ) -> Option<Self> {
        if !Self::eligible_receiver(from_shape)
            || object::shape_body::state_of(from_shape).is_prototype()
            || object::shape_body::id_of(from_shape) != transition.from_shape_id
        {
            return None;
        }
        let to_shape = transition.to_shape.get();
        let (guard, validity) = match &transition.kind {
            StorePropertyTransitionKind::OwnAdd => (TransitionGuard::OwnAdd, None),
            StorePropertyTransitionKind::PrototypeChainMissing { validity } => {
                (TransitionGuard::ChainMissing, Some(validity.clone()))
            }
            StorePropertyTransitionKind::PrototypeWritableData { validity } => {
                (TransitionGuard::PrototypeWritable, Some(validity.clone()))
            }
        };
        // A dictionary target has no layout to publish; its location is the
        // flat slot, consumed only by the VM's replay.
        let field = if to_shape.is_null() {
            FieldLocation::from_cache_key(0)
        } else {
            object::field_location(to_shape, u32::from(transition.slot))
        };
        Some(Self {
            receiver_shape: from_shape,
            lookup_start: false,
            kind: IcHandlerKind::StoreTransition,
            field,
            slot: transition.slot,
            aux: to_shape,
            validity,
            transition: guard,
        })
    }
}

#[cfg(test)]
impl IcHandler {
    /// A load handler naming an arbitrary handle, for slot bookkeeping tests
    /// that never probe, compile or collect.
    pub(crate) fn fixture_load(shape_offset: u32) -> Self {
        Self {
            // SAFETY: the fixture handle is never dereferenced.
            receiver_shape: unsafe { ShapeHandle::from_offset(shape_offset) },
            lookup_start: false,
            kind: IcHandlerKind::OwnField,
            field: FieldLocation::inline(0),
            slot: 0,
            aux: ShapeHandle::null(),
            validity: None,
            transition: TransitionGuard::OwnAdd,
        }
    }
}

/// Result of installing a handler into a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallOutcome {
    /// The slot now serves the handler's receiver shape.
    Installed,
    /// The slot overflowed and is now megamorphic.
    BecameMegamorphic,
    /// The slot is megamorphic; nothing changed.
    Unchanged,
}

/// Byte offsets of the native slot, read by both backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyIcLayout {
    /// `u32` shape of the first own-field entry, or [`EMPTY_SHAPE`].
    pub inline_shape_byte: u32,
    /// `u32` [`FieldLocation`] key of that entry.
    pub inline_field_byte: u32,
    /// `u32` state: entry count in the low byte, then flags.
    pub state_byte: u32,
    /// Bit set in `state` once the slot is megamorphic.
    pub megamorphic_bit: u32,
    /// Mask of the entry count in `state`.
    pub count_mask: u32,
    /// `u32` property atom.
    pub atom_byte: u32,
    /// First entry.
    pub entries_byte: u32,
    /// Bytes per entry.
    pub entry_bytes: u32,
    /// Entry `u32` receiver shape.
    pub entry_shape_byte: u32,
    /// Entry `u32` field key.
    pub entry_field_byte: u32,
    /// Entry `u8` handler kind.
    pub entry_kind_byte: u32,
    /// Entry `u32` auxiliary shape.
    pub entry_aux_byte: u32,
    /// Entry `u64` validity word address.
    pub entry_validity_byte: u32,
}

/// One named-property site: V8's `(feedback, extra)` slot pair with the
/// polymorphic array inline.
#[derive(Debug)]
#[repr(C)]
pub struct PropertyIcSlot {
    inline_shape: AtomicU32,
    inline_field: AtomicU32,
    state: AtomicU32,
    atom: AtomicU32,
    function_id: AtomicU32,
    instruction_pc: AtomicU32,
    entries: [IcEntry; PROFILED_PROPERTY_PIC_CAPACITY],
    hits: AtomicU32,
    misses: AtomicU32,
    installs: AtomicU32,
    disables: AtomicU32,
    kind: PropertyIcKind,
}

impl PropertyIcSlot {
    /// The layout generated code addresses.
    pub const LAYOUT: PropertyIcLayout = PropertyIcLayout {
        inline_shape_byte: std::mem::offset_of!(Self, inline_shape) as u32,
        inline_field_byte: std::mem::offset_of!(Self, inline_field) as u32,
        state_byte: std::mem::offset_of!(Self, state) as u32,
        megamorphic_bit: STATE_MEGAMORPHIC,
        count_mask: STATE_COUNT_MASK,
        atom_byte: std::mem::offset_of!(Self, atom) as u32,
        entries_byte: std::mem::offset_of!(Self, entries) as u32,
        entry_bytes: std::mem::size_of::<IcEntry>() as u32,
        entry_shape_byte: std::mem::offset_of!(IcEntry, shape) as u32,
        entry_field_byte: std::mem::offset_of!(IcEntry, field) as u32,
        entry_kind_byte: std::mem::offset_of!(IcEntry, kind) as u32,
        entry_aux_byte: std::mem::offset_of!(IcEntry, aux) as u32,
        entry_validity_byte: std::mem::offset_of!(IcEntry, validity) as u32,
    };

    /// A cold slot of `kind`.
    #[must_use]
    pub(crate) const fn new(kind: PropertyIcKind) -> Self {
        Self {
            inline_shape: AtomicU32::new(EMPTY_SHAPE),
            inline_field: AtomicU32::new(0),
            state: AtomicU32::new(0),
            atom: AtomicU32::new(0),
            function_id: AtomicU32::new(0),
            instruction_pc: AtomicU32::new(0),
            entries: [const { IcEntry::new() }; PROFILED_PROPERTY_PIC_CAPACITY],
            hits: AtomicU32::new(0),
            misses: AtomicU32::new(0),
            installs: AtomicU32::new(0),
            disables: AtomicU32::new(0),
            kind,
        }
    }

    /// Address generated code embeds for this site.
    #[must_use]
    pub(crate) fn address(&self) -> u64 {
        std::ptr::from_ref(self) as u64
    }

    /// Record the site identity the generated miss decodes. Idempotent: a
    /// site's function, pc and atom never change after linking.
    pub(crate) fn bind_site(&self, function_id: u32, instruction_pc: u32, atom: u32) {
        self.function_id.store(function_id, Ordering::Relaxed);
        self.instruction_pc.store(instruction_pc, Ordering::Relaxed);
        self.atom.store(atom, Ordering::Relaxed);
    }

    /// The `(function id, instruction pc)` recorded by [`Self::bind_site`].
    #[must_use]
    pub(crate) fn site(&self) -> (u32, u32) {
        (
            self.function_id.load(Ordering::Relaxed),
            self.instruction_pc.load(Ordering::Relaxed),
        )
    }

    /// Opcode family.
    #[must_use]
    pub(crate) fn kind(&self) -> PropertyIcKind {
        self.kind
    }

    fn state(&self) -> u32 {
        self.state.load(Ordering::Relaxed)
    }

    /// Number of live entries.
    #[must_use]
    pub(crate) fn entry_count(&self) -> usize {
        (self.state() & STATE_COUNT_MASK) as usize
    }

    /// Whether the site overflowed into the shared table.
    #[must_use]
    pub(crate) fn is_megamorphic(&self) -> bool {
        self.state() & STATE_MEGAMORPHIC != 0
    }

    /// Whether property dispatch has run at this site.
    #[must_use]
    pub(crate) fn attempted(&self) -> bool {
        self.state() & STATE_ATTEMPTED != 0
    }

    /// Mark the first dispatch. Returns whether this was it.
    pub(crate) fn record_attempt(&self) -> bool {
        let state = self.state();
        if state & STATE_ATTEMPTED != 0 {
            return false;
        }
        self.state.store(state | STATE_ATTEMPTED, Ordering::Relaxed);
        true
    }

    fn set_count(&self, count: usize) {
        let state = self.state() & !STATE_COUNT_MASK;
        self.state.store(state | count as u32, Ordering::Relaxed);
        self.refresh_inline();
    }

    /// Republish the generated fast path's own-field pair: the first entry
    /// whose handler is the site's own-field kind.
    fn refresh_inline(&self) {
        let own = match self.kind {
            PropertyIcKind::Load => IcHandlerKind::OwnField,
            PropertyIcKind::Store => IcHandlerKind::StoreField,
        };
        let entry = self
            .live()
            .iter()
            .find(|entry| entry.kind() == own && !entry.lookup_start());
        match entry {
            Some(entry) => {
                self.inline_field
                    .store(entry.field.load(Ordering::Relaxed), Ordering::Relaxed);
                self.inline_shape.store(entry.shape(), Ordering::Relaxed);
            }
            None => {
                self.inline_shape.store(EMPTY_SHAPE, Ordering::Relaxed);
                self.inline_field.store(0, Ordering::Relaxed);
            }
        }
    }

    fn live(&self) -> &[IcEntry] {
        &self.entries[..self.entry_count()]
    }

    fn matching(&self, shape: u32) -> Option<&IcEntry> {
        self.live().iter().find(|entry| entry.shape() == shape)
    }

    /// Run the load handler of `obj`'s shape. `None` on a miss.
    #[must_use]
    pub(crate) fn probe_load(&self, obj: JsObject, heap: &otter_gc::GcHeap) -> Option<Value> {
        self.probe_load_from(obj, object::shape(obj, heap).offset(), heap)
    }

    /// Whether the load handler of `obj`'s shape finds the key: a field of the
    /// object or its chain (`true`) or a proven absence (`false`). `None` on a
    /// miss. Presence reads no value, so the entry that answers `[[Get]]`
    /// answers `in` too.
    #[must_use]
    pub(crate) fn probe_has(&self, obj: JsObject, heap: &otter_gc::GcHeap) -> Option<bool> {
        let entry = self.matching(object::shape(obj, heap).offset())?;
        match entry.kind() {
            IcHandlerKind::OwnField => Some(true),
            IcHandlerKind::PrototypeField => entry.proof_holds().then_some(true),
            IcHandlerKind::NonExistent => entry.proof_holds().then_some(false),
            _ => None,
        }
    }

    /// Run the load handler of a receiver whose named lookup starts at
    /// `start`. `None` on a miss.
    #[must_use]
    pub(crate) fn probe_lookup_start_load(
        &self,
        start: JsObject,
        heap: &otter_gc::GcHeap,
    ) -> Option<Value> {
        let shape = object::keyed_shape(start, heap);
        if shape.is_null() {
            return None;
        }
        self.probe_load_from(start, shape.offset() | LOOKUP_START_KEY_BIT, heap)
    }

    /// Run the load handler keyed `key` with `obj` as lookup-start object.
    fn probe_load_from(&self, obj: JsObject, key: u32, heap: &otter_gc::GcHeap) -> Option<Value> {
        let entry = self.matching(key)?;
        match entry.kind() {
            IcHandlerKind::OwnField => Some(object::load_proven_data_slot(obj, heap, entry.slot())),
            IcHandlerKind::PrototypeField => {
                if !entry.proof_holds() {
                    return None;
                }
                let object::shape_body::ShapePrototype::Object(holder) =
                    object::shape_body::prototype_of(entry.aux())
                else {
                    return None;
                };
                Some(object::load_proven_data_slot(holder, heap, entry.slot()))
            }
            IcHandlerKind::NonExistent => entry.proof_holds().then(Value::undefined),
            _ => None,
        }
    }

    /// Run the store handler of `obj`'s shape. `Ok(false)` on a miss, which
    /// allocates nothing; OOM after a matched transition propagates.
    pub(crate) fn probe_store(
        &self,
        obj: JsObject,
        heap: &mut otter_gc::GcHeap,
        key: AtomizedPropertyKey<'_>,
        value: &Value,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        let Some(entry) = self.matching(object::shape(obj, heap).offset()) else {
            return Ok(false);
        };
        match entry.kind() {
            IcHandlerKind::StoreField => {
                object::store_proven_data_slot(obj, heap, entry.slot(), *value);
                Ok(true)
            }
            IcHandlerKind::StoreTransition => {
                let word = entry.validity_word();
                let kind = match entry.transition_guard() {
                    TransitionGuard::OwnAdd => StorePropertyTransitionKind::OwnAdd,
                    // SAFETY: a non-zero word owns a strong count.
                    TransitionGuard::ChainMissing if word != 0 => {
                        StorePropertyTransitionKind::PrototypeChainMissing {
                            validity: unsafe { PrototypeValidity::clone_raw(word) },
                        }
                    }
                    // SAFETY: as above.
                    TransitionGuard::PrototypeWritable if word != 0 => {
                        StorePropertyTransitionKind::PrototypeWritableData {
                            validity: unsafe { PrototypeValidity::clone_raw(word) },
                        }
                    }
                    _ => return Ok(false),
                };
                let from = object::shape_body::id_of(entry.shape_handle());
                let target = entry.aux();
                let target_id = if target.is_null() {
                    object::ShapeId::UNASSIGNED
                } else {
                    object::shape_body::id_of(target)
                };
                Ok(object::replay_store_property_transition(
                    obj,
                    heap,
                    key,
                    from,
                    key.atom().id(),
                    target_id,
                    || target,
                    &kind,
                    entry.slot(),
                    value,
                )?
                .is_some())
            }
            _ => Ok(false),
        }
    }

    /// Install `handler`: V8 `IC::SetCache` for a non-global named IC.
    ///
    /// An entry for the same receiver shape is replaced (healing a handler
    /// whose proof died); entries with dead proofs are dropped; a new shape is
    /// appended while capacity remains, otherwise the slot becomes megamorphic.
    pub(crate) fn install(&self, handler: IcHandler) -> InstallOutcome {
        if self.is_megamorphic() {
            return InstallOutcome::Unchanged;
        }
        debug_assert_eq!(
            handler.kind.is_store(),
            self.kind == PropertyIcKind::Store,
            "handler family matches the site"
        );
        if let Some(entry) = self.matching(handler.key()) {
            entry.write(handler);
            self.refresh_inline();
            self.installs.fetch_add(1, Ordering::Relaxed);
            return InstallOutcome::Installed;
        }
        self.retain(|entry| entry.proof_holds());
        let count = self.entry_count();
        if count < PROFILED_PROPERTY_PIC_CAPACITY {
            self.entries[count].write(handler);
            self.set_count(count + 1);
            self.installs.fetch_add(1, Ordering::Relaxed);
            return InstallOutcome::Installed;
        }
        for entry in self.live() {
            entry.clear();
        }
        let state = (self.state() & !STATE_COUNT_MASK) | STATE_MEGAMORPHIC;
        self.state.store(state, Ordering::Relaxed);
        self.refresh_inline();
        self.disables.fetch_add(1, Ordering::Relaxed);
        InstallOutcome::BecameMegamorphic
    }

    /// Keep only entries `keep` accepts, compacting in order.
    fn retain(&self, mut keep: impl FnMut(&IcEntry) -> bool) {
        let count = self.entry_count();
        let mut kept = 0;
        for index in 0..count {
            let entry = &self.entries[index];
            if !keep(entry) {
                entry.clear();
                continue;
            }
            if kept != index {
                self.entries[kept].take_from(entry);
            }
            kept += 1;
        }
        if kept != count {
            self.set_count(kept);
        }
    }

    /// Full-collector weak pass: forget entries naming unmarked shapes.
    pub(crate) fn sweep_dead(&self, heap: &otter_gc::GcHeap) {
        if self.entry_count() == 0 {
            return;
        }
        let live = |handle: u32| {
            // SAFETY: entry words hold shape handles published by `write`.
            heap.is_marked(unsafe { ShapeHandle::from_offset(handle) }.raw())
        };
        self.retain(|entry| {
            live(entry.shape() & !LOOKUP_START_KEY_BIT)
                && (entry.aux.load(Ordering::Relaxed) == 0
                    || live(entry.aux.load(Ordering::Relaxed)))
        });
    }

    /// The first live own-field load entry: `(receiver shape, logical slot)`.
    #[must_use]
    pub(crate) fn mono_own_field(&self) -> Option<(ShapeHandle, u16)> {
        let [entry] = self.live() else {
            return None;
        };
        (entry.kind() == IcHandlerKind::OwnField && !entry.lookup_start())
            .then(|| (entry.shape_handle(), entry.slot()))
    }

    /// Count one VM-served hit.
    pub(crate) fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one miss of an object receiver.
    pub(crate) fn record_miss(&self) {
        if !self.is_megamorphic() {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counter snapshot attributed to the site's family.
    #[must_use]
    pub(crate) fn stats(&self) -> PropertyIcStats {
        let hits = u64::from(self.hits.load(Ordering::Relaxed));
        let misses = u64::from(self.misses.load(Ordering::Relaxed));
        let installs = u64::from(self.installs.load(Ordering::Relaxed));
        let disables = u64::from(self.disables.load(Ordering::Relaxed));
        match self.kind {
            PropertyIcKind::Load => PropertyIcStats {
                load_hits: hits,
                load_misses: misses,
                load_installs: installs,
                load_disables: disables,
                ..PropertyIcStats::default()
            },
            PropertyIcKind::Store => PropertyIcStats {
                store_hits: hits,
                store_misses: misses,
                store_installs: installs,
                store_disables: disables,
                ..PropertyIcStats::default()
            },
        }
    }

    /// Devtools view of the site.
    #[must_use]
    pub(crate) fn snapshot_state(&self) -> crate::inspect::IcSiteState {
        use crate::inspect::{IcEntrySnapshot, IcEntryVariant, IcSiteState};
        if self.is_megamorphic() {
            return IcSiteState::Megamorphic;
        }
        if self.entry_count() == 0 {
            return if self.attempted() {
                IcSiteState::Uncacheable
            } else {
                IcSiteState::Empty
            };
        }
        let entries = self
            .live()
            .iter()
            .map(|entry| {
                let receiver = entry.shape_handle();
                let variant = match (entry.kind(), entry.transition_guard()) {
                    (IcHandlerKind::OwnField | IcHandlerKind::StoreField, _) => {
                        IcEntryVariant::OwnData
                    }
                    (IcHandlerKind::PrototypeField, _) => IcEntryVariant::InheritedData,
                    (IcHandlerKind::NonExistent, _) => IcEntryVariant::NonExistent,
                    (IcHandlerKind::StoreTransition, TransitionGuard::OwnAdd) => {
                        IcEntryVariant::OwnAddTransition
                    }
                    (IcHandlerKind::StoreTransition, TransitionGuard::ChainMissing) => {
                        IcEntryVariant::PrototypeChainMissingTransition
                    }
                    (IcHandlerKind::StoreTransition, TransitionGuard::PrototypeWritable) => {
                        IcEntryVariant::PrototypeWritableDataTransition
                    }
                    (IcHandlerKind::Empty, _) => IcEntryVariant::OwnData,
                };
                let transition = entry.kind() == IcHandlerKind::StoreTransition;
                IcEntrySnapshot {
                    variant,
                    receiver_shape_id: object::shape_body::id_of(receiver).raw(),
                    key: None,
                    slot: (!transition && entry.kind() != IcHandlerKind::NonExistent)
                        .then(|| entry.slot()),
                    to_shape_id: (transition && !entry.aux().is_null())
                        .then(|| object::shape_body::id_of(entry.aux()).raw()),
                }
            })
            .collect();
        IcSiteState::Polymorphic { entries }
    }

    /// Copy every entry the optimizing tier can lower into compile metadata.
    ///
    /// `bake_shape` names a shape in the compilation (and retains it);
    /// `bake_validity` retains a still-valid proof. An entry whose shape can
    /// no longer be named or whose proof died is dropped, as are entries with
    /// no CacheIR lowering (nonexistent keys, dictionary transitions): their
    /// receivers reach the optimized code like unseen shapes. `None` when
    /// nothing remains.
    pub(crate) fn jit_programs(
        &self,
        atom: u32,
        mut bake_shape: impl FnMut(ShapeHandle) -> Option<u32>,
        mut bake_validity: impl FnMut(
            &Arc<PrototypeValidity>,
        ) -> Option<crate::jit::JitPrototypeValidity>,
    ) -> Option<Vec<crate::jit::JitCacheIrProgram>> {
        use crate::jit::JitCacheIrOp;
        let mut programs = Vec::with_capacity(self.entry_count());
        for entry in self.live() {
            // Lookup-start receivers have no CacheIR lowering in the optimizing
            // tier; they reach its generic node like unseen shapes.
            if entry.lookup_start() {
                continue;
            }
            let receiver = entry.shape_handle();
            let Some(shape) = bake_shape(receiver) else {
                continue;
            };
            let validity = match entry.validity_word() {
                0 => None,
                word => {
                    // SAFETY: a non-zero word owns a strong count.
                    let cell = unsafe { PrototypeValidity::clone_raw(word) };
                    match bake_validity(&cell) {
                        Some(validity) => Some(validity),
                        None => continue,
                    }
                }
            };
            let field = entry.field();
            let ops: Vec<JitCacheIrOp> = match entry.kind() {
                IcHandlerKind::OwnField => vec![
                    JitCacheIrOp::GuardShape { object: 0, shape },
                    JitCacheIrOp::GuardAtomSlot {
                        object: 0,
                        atom,
                        field,
                        writable: false,
                    },
                    JitCacheIrOp::LoadField { object: 0, field },
                ],
                IcHandlerKind::PrototypeField => {
                    let (Some(root), Some(validity)) = (bake_shape(entry.aux()), validity) else {
                        continue;
                    };
                    vec![
                        JitCacheIrOp::GuardShape { object: 0, shape },
                        JitCacheIrOp::GuardPrototypeValidity { validity },
                        JitCacheIrOp::LoadPrototypeHolder { root, result: 1 },
                        JitCacheIrOp::LoadField { object: 1, field },
                    ]
                }
                IcHandlerKind::StoreField => vec![
                    JitCacheIrOp::GuardShape { object: 0, shape },
                    JitCacheIrOp::GuardAtomSlot {
                        object: 0,
                        atom,
                        field,
                        writable: true,
                    },
                    JitCacheIrOp::StoreField { object: 0, field },
                ],
                IcHandlerKind::StoreTransition => {
                    if entry.aux().is_null() {
                        continue;
                    }
                    let Some(to_shape) = bake_shape(entry.aux()) else {
                        continue;
                    };
                    let mut ops = vec![JitCacheIrOp::GuardShape { object: 0, shape }];
                    match (entry.transition_guard(), validity) {
                        (TransitionGuard::OwnAdd, _) => {
                            ops.push(JitCacheIrOp::GuardPrototypeNull { object: 0 });
                        }
                        (_, Some(validity)) => {
                            ops.push(JitCacheIrOp::GuardPrototypeValidity { validity });
                        }
                        (_, None) => continue,
                    }
                    ops.push(JitCacheIrOp::GuardExtensible { object: 0, field });
                    ops.push(JitCacheIrOp::StoreField { object: 0, field });
                    ops.push(JitCacheIrOp::PublishShape {
                        object: 0,
                        shape: to_shape,
                    });
                    ops
                }
                IcHandlerKind::NonExistent | IcHandlerKind::Empty => continue,
            };
            programs.push(crate::jit::JitCacheIrProgram {
                ops: ops.into_boxed_slice(),
            });
        }
        (!programs.is_empty()).then_some(programs)
    }
}

impl Drop for PropertyIcSlot {
    fn drop(&mut self) {
        for entry in &self.entries {
            entry.clear();
        }
    }
}

const _: () = {
    assert!(std::mem::size_of::<IcEntry>() == 24);
    assert!(std::mem::offset_of!(PropertyIcSlot, inline_shape) == 0);
    assert!(std::mem::offset_of!(PropertyIcSlot, inline_field) == 4);
};

#[cfg(test)]
#[path = "property_ic/tests.rs"]
mod tests;
