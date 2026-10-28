//! Per-site named-property inline caches: a feedback slot and handlers.
//!
//! Each `LoadProperty`, `StoreProperty` and the method lookup of
//! `CallMethodValue` owns one [`PropertyIcSlot`] in its CodeBlock's feedback
//! vector. The slot is mutable data that the VM and generated code both read:
//! the receiver's shape selects an [`IcEntry`] whose handler says how to finish
//! the access. Misses update the slot through the IC state machine;
//! generated code never changes when the feedback does.
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
//! - One checked [`PropertyStorageProjection`] occupies the low two bits of
//!   an 8-aligned storage shape key. Empty keys are rejected before decoding;
//!   the reserved projection and unaligned or null shapes cannot be installed.
//!   Receiver handlers operate on the source object. Lookup-start handlers
//!   operate on a function bag or a proved clean exotic prototype. Array-expando
//!   handlers reload the array's live own storage and admit only own data loads
//!   and overwrites: its null prototype never proves absence, inheritance or an
//!   append on the original array. The projection never replaces the JS receiver.
//! - A matched receiver shape fixes the object's lookup state, own keys,
//!   attributes, prototype and storage banks. Prototype-field and
//!   nonexistent handlers additionally require their chain proof; a
//!   transition requires its proof (or a `null` prototype, which the receiver
//!   shape fixes). An invalid proof only misses.
//! - Store handlers are never installed for prototype-role receivers: their
//!   mutations must reach the invalidation boundary.
//! - Shaped store owner words name the defining ancestor of the live logical
//!   slot. Zero skips observation only for an absorbing Top summary. An append
//!   with a null target has zero owner and flat field zero for VM dictionary
//!   replay; generated probes reject that target before any effect.
//! - Every data slot accepts any valid Value word. Reads preserve its encoded
//!   representation; stores observe its incoming physical category.
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

/// What a matched entry does: the load and store handler kinds for Otter's
/// ordinary objects.
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
    /// Call the getter of the accessor the holder `prototype(aux)` owns,
    /// authorized by the entry's chain proof.
    PrototypeAccessor = 6,
    /// Call the getter of the receiver's own accessor slot, which its shape
    /// guarantees holds an accessor.
    OwnAccessor = 7,
}

impl IcHandlerKind {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::OwnField,
            2 => Self::PrototypeField,
            3 => Self::NonExistent,
            4 => Self::StoreField,
            5 => Self::StoreTransition,
            6 => Self::PrototypeAccessor,
            7 => Self::OwnAccessor,
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

/// Storage selected before a property handler runs. The source receiver remains
/// unchanged; projected objects own only the storage operation.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PropertyStorageProjection {
    /// The source receiver owns the shape and storage operation.
    #[default]
    Receiver = 0,
    /// A separately proved lookup-start object owns the operation.
    LookupStart = 1,
    /// The array's live expando owns an existing non-index data slot.
    ArrayExpando = 2,
    /// A typed array's proven lookup start (its body's `lookup_start`) owns
    /// the operation: an own data slot of that prototype or one it inherits.
    TypedArrayStart = 3,
}

impl PropertyStorageProjection {
    /// Bits reserved in an aligned shape key for its storage projection.
    pub const KEY_MASK: u32 = 3;

    /// Encode one nonzero aligned shape with this projection.
    pub fn encode_key(self, shape: u32) -> u32 {
        debug_assert!(shape != 0 && shape.is_multiple_of(8));
        shape | self as u32
    }

    /// Decode the current key format, rejecting emptiness and reserved codes.
    pub fn decode_key(key: u32) -> Option<(Self, u32)> {
        if key == EMPTY_SHAPE {
            return None;
        }
        let projection = match key & Self::KEY_MASK {
            0 => Self::Receiver,
            1 => Self::LookupStart,
            2 => Self::ArrayExpando,
            _ => Self::TypedArrayStart,
        };
        let shape = key & !Self::KEY_MASK;
        (shape != 0 && shape.is_multiple_of(8)).then_some((projection, shape))
    }
}

const STATE_COUNT_MASK: u32 = 0xff;
const STATE_ATTEMPTED: u32 = 1 << 8;
const STATE_MEGAMORPHIC: u32 = 1 << 9;
/// An element site's cache serves the one name key its `atom` holds.
const STATE_KEYED_BOUND: u32 = 1 << 13;
const STATE_LENGTH_NON_ARRAY: u32 = 1 << 10;
const STATE_LENGTH_NON_TYPED_ARRAY: u32 = 1 << 11;
const STATE_LENGTH_NON_STRING: u32 = 1 << 12;

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
    owner: AtomicU32,

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
            owner: AtomicU32::new(0),

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

    /// Whether the entry already holds exactly `handler` for its key: the
    /// same field, kind, transition guard, slot, auxiliary shape and proof.
    fn holds(&self, handler: &IcHandler) -> bool {
        let validity = handler
            .validity
            .as_ref()
            .map_or(0, |cell| Arc::as_ptr(cell) as u64);
        self.field.load(Ordering::Relaxed) == handler.field.cache_key()
            && self.kind() == handler.kind
            && self.transition.load(Ordering::Relaxed) == handler.transition as u8
            && self.slot() == handler.slot
            && self.aux.load(Ordering::Relaxed) == handler.aux.offset()
            && self.owner.load(Ordering::Relaxed) == handler.owner.offset()
            && self.validity_word() == validity
    }

    /// Write `handler`; the proof word the entry held, whose count the
    /// caller now owns.
    #[must_use]
    fn write(&self, handler: IcHandler) -> u64 {
        let key = handler.key();
        let old = self.validity.swap(
            handler.validity.map_or(0, PrototypeValidity::into_raw),
            Ordering::Relaxed,
        );
        self.field
            .store(handler.field.cache_key(), Ordering::Relaxed);
        self.kind.store(handler.kind as u8, Ordering::Relaxed);
        self.transition
            .store(handler.transition as u8, Ordering::Relaxed);
        self.slot.store(handler.slot, Ordering::Relaxed);
        self.aux.store(handler.aux.offset(), Ordering::Relaxed);
        self.owner.store(handler.owner.offset(), Ordering::Relaxed);
        self.shape.store(key, Ordering::Relaxed);
        old
    }

    /// A copy of every word, owning no proof count.
    fn copy(&self) -> Self {
        Self {
            shape: AtomicU32::new(self.shape.load(Ordering::Relaxed)),
            field: AtomicU32::new(self.field.load(Ordering::Relaxed)),
            kind: AtomicU8::new(self.kind.load(Ordering::Relaxed)),
            transition: AtomicU8::new(self.transition.load(Ordering::Relaxed)),
            slot: AtomicU16::new(self.slot.load(Ordering::Relaxed)),
            aux: AtomicU32::new(self.aux.load(Ordering::Relaxed)),
            owner: AtomicU32::new(self.owner.load(Ordering::Relaxed)),
            validity: AtomicU64::new(self.validity.load(Ordering::Relaxed)),
        }
    }

    /// The aligned shape named by this validated live entry.
    fn shape_handle(&self) -> ShapeHandle {
        let (_, shape) = PropertyStorageProjection::decode_key(self.shape())
            .expect("live property entry has a valid shape projection");
        // SAFETY: writes encode rooted shape handles; the weak pass clears dead entries.
        unsafe { ShapeHandle::from_offset(shape) }
    }

    fn projection(&self) -> PropertyStorageProjection {
        PropertyStorageProjection::decode_key(self.shape())
            .expect("live property entry has a valid storage projection")
            .0
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
        self.owner
            .store(other.owner.load(Ordering::Relaxed), Ordering::Relaxed);
        self.validity
            .store(other.validity.swap(0, Ordering::Relaxed), Ordering::Relaxed);
        other.reset();
    }

    /// Empty the entry; the proof word it held, whose count the caller now
    /// owns.
    #[must_use]
    fn clear(&self) -> u64 {
        let old = self.validity.swap(0, Ordering::Relaxed);
        self.reset();
        old
    }

    fn reset(&self) {
        self.shape.store(EMPTY_SHAPE, Ordering::Relaxed);
        self.field.store(0, Ordering::Relaxed);
        self.kind
            .store(IcHandlerKind::Empty as u8, Ordering::Relaxed);
        self.transition.store(0, Ordering::Relaxed);
        self.slot.store(0, Ordering::Relaxed);
        self.aux.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Relaxed);
    }

    fn transition_guard(&self) -> TransitionGuard {
        match self.transition.load(Ordering::Relaxed) {
            1 => TransitionGuard::ChainMissing,
            2 => TransitionGuard::PrototypeWritable,
            _ => TransitionGuard::OwnAdd,
        }
    }
}

/// What a load handler answered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum LoadHit {
    /// The property's value.
    Value(Value),
    /// The getter of an inherited accessor, for the caller to call with the
    /// original receiver.
    Getter(Value),
}

impl LoadHit {
    /// The value of a data or absent answer.
    #[must_use]
    pub(crate) fn value(self) -> Option<Value> {
        match self {
            Self::Value(value) => Some(value),
            Self::Getter(_) => None,
        }
    }
}

/// A handler ready to install: what an [`IcEntry`] holds, owned by Rust.
#[derive(Debug, Clone)]
pub(crate) struct IcHandler {
    receiver_shape: ShapeHandle,
    projection: PropertyStorageProjection,
    kind: IcHandlerKind,
    field: FieldLocation,
    slot: u16,
    aux: ShapeHandle,
    owner: ShapeHandle,

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

    /// Whether an own named slot of a receiver with `shape` is read and
    /// written by its layout alone: a non-ordinary `[[Prototype]]` (a
    /// function value) decides only the keys the receiver lacks.
    fn eligible_own_receiver(shape: ShapeHandle) -> bool {
        use object::ShapeState;
        if shape.is_null() {
            return false;
        }
        let state = object::shape_body::state_of(shape);
        let facts = ShapeState::OPAQUE_LOOKUP_MASK & !ShapeState::OPAQUE_PROTOTYPE_MASK;
        !state.is_dictionary() && state.bits() & facts == 0
    }

    /// The load handler for a resolved own or inherited data slot of a
    /// receiver whose layout is `receiver_shape`.
    #[must_use]
    pub(crate) fn load_resolved(
        receiver_shape: ShapeHandle,
        resolved: &crate::cache_ir::ResolvedDataSlot,
    ) -> Option<Self> {
        if resolved.hit.shape.is_null() {
            return None;
        }
        let eligible = if resolved.hops == 0 {
            Self::eligible_own_receiver(receiver_shape)
        } else {
            Self::eligible_receiver(receiver_shape)
        };
        if !eligible {
            return None;
        }
        let field = object::field_location(resolved.hit.shape, u32::from(resolved.hit.slot));
        if resolved.hops == 0 {
            if resolved.hit.shape != receiver_shape {
                return None;
            }
            return Some(Self {
                receiver_shape,
                projection: PropertyStorageProjection::Receiver,
                kind: IcHandlerKind::OwnField,
                field,
                slot: resolved.hit.slot,
                aux: ShapeHandle::null(),
                owner: ShapeHandle::null(),

                validity: None,
                transition: TransitionGuard::OwnAdd,
            });
        }
        if resolved.holder_root.is_null() {
            return None;
        }
        Some(Self {
            receiver_shape,
            projection: PropertyStorageProjection::Receiver,
            kind: IcHandlerKind::PrototypeField,
            field,
            slot: resolved.hit.slot,
            aux: resolved.holder_root,
            owner: ShapeHandle::null(),

            validity: Some(resolved.validity.clone()?),
            transition: TransitionGuard::OwnAdd,
        })
    }

    /// The load handler for a getter a prototype of the receiver owns.
    #[must_use]
    pub(crate) fn load_inherited_accessor(
        receiver_shape: ShapeHandle,
        resolved: &crate::cache_ir::ResolvedAccessorSlot,
    ) -> Option<Self> {
        if !Self::eligible_receiver(receiver_shape)
            || resolved.hit.shape.is_null()
            || resolved.holder_root.is_null()
            || resolved.getter.is_none()
        {
            return None;
        }
        Some(Self {
            receiver_shape,
            projection: PropertyStorageProjection::Receiver,
            kind: IcHandlerKind::PrototypeAccessor,
            field: object::field_location(resolved.hit.shape, u32::from(resolved.hit.slot)),
            slot: resolved.hit.slot,
            aux: resolved.holder_root,
            owner: ShapeHandle::null(),

            validity: Some(resolved.validity.clone()),
            transition: TransitionGuard::OwnAdd,
        })
    }

    /// The load handler for an accessor the receiver owns at `hit`, read
    /// from the slot on every hit.
    #[must_use]
    pub(crate) fn load_own_accessor(
        receiver_shape: ShapeHandle,
        hit: &crate::object::AtomOwnPropertyHit,
    ) -> Option<Self> {
        if !Self::eligible_receiver(receiver_shape) || hit.shape != receiver_shape {
            return None;
        }
        Some(Self {
            receiver_shape,
            projection: PropertyStorageProjection::Receiver,
            kind: IcHandlerKind::OwnAccessor,
            field: object::field_location(hit.shape, u32::from(hit.slot)),
            slot: hit.slot,
            aux: ShapeHandle::null(),
            owner: ShapeHandle::null(),

            validity: None,
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
            projection: PropertyStorageProjection::Receiver,
            kind: IcHandlerKind::NonExistent,
            field: FieldLocation::from_cache_key(0),
            slot: 0,
            aux: ShapeHandle::null(),
            owner: ShapeHandle::null(),

            validity,
            transition: TransitionGuard::OwnAdd,
        })
    }

    /// Select the object's role without granting array-chain absence or append.
    #[must_use]
    pub(crate) fn projected(mut self, projection: PropertyStorageProjection) -> Option<Self> {
        if (projection == PropertyStorageProjection::LookupStart
            && self.kind == IcHandlerKind::StoreTransition)
            || (projection == PropertyStorageProjection::TypedArrayStart
                && matches!(
                    self.kind,
                    IcHandlerKind::StoreField | IcHandlerKind::StoreTransition
                ))
            || (projection == PropertyStorageProjection::ArrayExpando
                && !matches!(
                    self.kind,
                    IcHandlerKind::OwnField | IcHandlerKind::StoreField
                ))
        {
            return None;
        }
        self.projection = projection;
        Some(self)
    }

    fn key(&self) -> u32 {
        self.projection.encode_key(self.receiver_shape.offset())
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
        if !Self::eligible_own_receiver(receiver_shape)
            || object::shape_body::state_of(receiver_shape).is_prototype()
        {
            return None;
        }
        let lookup = object::lookup_own_atom(obj, heap, key);
        let (Some(hit), object::PropertyLookup::Data { flags, .. }) = (lookup.hit, lookup.lookup)
        else {
            return None;
        };
        if !flags.writable() || hit.shape != receiver_shape {
            return None;
        }
        let owner = object::field_type::observer_owner(receiver_shape, u32::from(hit.slot))?;
        Some((
            Self {
                receiver_shape,
                projection: PropertyStorageProjection::Receiver,
                kind: IcHandlerKind::StoreField,
                field: object::field_location(receiver_shape, u32::from(hit.slot)),
                slot: hit.slot,
                aux: ShapeHandle::null(),
                owner,

                validity: None,
                transition: TransitionGuard::OwnAdd,
            },
            hit,
        ))
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
        if !to_shape.is_null() && object::shape_body::parent_of(to_shape) != from_shape {
            return None;
        }
        let (owner, field) = if to_shape.is_null() {
            (ShapeHandle::null(), FieldLocation::from_cache_key(0))
        } else {
            (
                object::field_type::observer_owner(to_shape, u32::from(transition.slot))?,
                object::field_location(to_shape, u32::from(transition.slot)),
            )
        };
        let (guard, validity) = match &transition.kind {
            StorePropertyTransitionKind::OwnAdd => (TransitionGuard::OwnAdd, None),
            StorePropertyTransitionKind::PrototypeChainMissing { validity } => {
                (TransitionGuard::ChainMissing, Some(validity.clone()))
            }
            StorePropertyTransitionKind::PrototypeWritableData { validity } => {
                (TransitionGuard::PrototypeWritable, Some(validity.clone()))
            }
        };
        Some(Self {
            receiver_shape: from_shape,
            projection: PropertyStorageProjection::Receiver,
            kind: IcHandlerKind::StoreTransition,
            field,
            slot: transition.slot,
            aux: to_shape,
            owner,
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
            projection: PropertyStorageProjection::Receiver,
            kind: IcHandlerKind::OwnField,
            field: FieldLocation::inline(0),
            slot: 0,
            aux: ShapeHandle::null(),
            owner: ShapeHandle::null(),

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
    /// `u32` defining store owner, or zero for an absorbing Top summary.
    pub inline_owner_byte: u32,
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
    /// Entry `u32` defining store owner: zero for shaped Top or a VM-only
    /// dictionary append with a null auxiliary target.
    pub entry_owner_byte: u32,
    /// Entry `u64` validity word address.
    pub entry_validity_byte: u32,
}

/// One named-property site: a `(feedback, extra)` slot pair with the
/// polymorphic array inline.
#[derive(Debug)]
#[repr(C)]
pub struct PropertyIcSlot {
    inline_shape: AtomicU32,
    inline_field: AtomicU32,
    inline_owner: AtomicU32,
    state: AtomicU32,
    atom: AtomicU32,
    function_id: AtomicU32,
    instruction_pc: AtomicU32,
    entries: [IcEntry; PROFILED_PROPERTY_PIC_CAPACITY],
    hits: AtomicU32,
    misses: AtomicU32,
    installs: AtomicU32,
    disables: AtomicU32,
    /// The integer index a keyed-bound site's atom spells; [`u32::MAX`] when
    /// it names a symbol.
    keyed_index: AtomicU32,
    /// Odd while the mutator rewrites the entries; a compiler thread's copy
    /// is consistent only between two equal even readings.
    version: AtomicU32,
    /// Compiler threads copying the entries now.
    readers: AtomicU32,
    /// One proof word dropped while a compiler thread was copying, released
    /// once none is.
    retired: AtomicU64,
    kind: PropertyIcKind,
}

impl PropertyIcSlot {
    /// The layout generated code addresses.
    pub const LAYOUT: PropertyIcLayout = PropertyIcLayout {
        inline_shape_byte: std::mem::offset_of!(Self, inline_shape) as u32,
        inline_field_byte: std::mem::offset_of!(Self, inline_field) as u32,
        inline_owner_byte: std::mem::offset_of!(Self, inline_owner) as u32,

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
        entry_owner_byte: std::mem::offset_of!(IcEntry, owner) as u32,

        entry_validity_byte: std::mem::offset_of!(IcEntry, validity) as u32,
    };

    /// A cold slot of `kind`.
    #[must_use]
    pub(crate) const fn new(kind: PropertyIcKind) -> Self {
        Self {
            inline_shape: AtomicU32::new(EMPTY_SHAPE),
            inline_field: AtomicU32::new(0),
            inline_owner: AtomicU32::new(0),
            state: AtomicU32::new(0),
            atom: AtomicU32::new(0),
            function_id: AtomicU32::new(0),
            instruction_pc: AtomicU32::new(0),

            entries: [const { IcEntry::new() }; PROFILED_PROPERTY_PIC_CAPACITY],
            hits: AtomicU32::new(0),
            misses: AtomicU32::new(0),
            installs: AtomicU32::new(0),
            disables: AtomicU32::new(0),
            keyed_index: AtomicU32::new(u32::MAX),
            version: AtomicU32::new(0),
            readers: AtomicU32::new(0),
            retired: AtomicU64::new(0),
            kind,
        }
    }

    /// Start rewriting the entries: the version turns odd.
    fn begin_write(&self) {
        let version = self.version.load(Ordering::Relaxed);
        self.version
            .store(version.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
    }

    /// Finish rewriting the entries: the version turns even again.
    fn end_write(&self) {
        let version = self.version.load(Ordering::Relaxed);
        self.version
            .store(version.wrapping_add(1), Ordering::Release);
    }

    /// Release the count of a proof word an entry dropped, unless a compiler
    /// thread may be taking it from its copy: then it waits in `retired`
    /// (or, with that taken, the short copy finishes first).
    fn release_validity(&self, word: u64) {
        if word == 0 {
            return;
        }
        // Pairs with the fence in `consistent_entries`: either the copier
        // sees the entry without `word`, or this sees the copier.
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.readers.load(Ordering::Relaxed) == 0 {
            // SAFETY: the dropped word owned its count.
            unsafe { PrototypeValidity::release_raw(word) };
            self.release_retired();
            return;
        }
        if self
            .retired
            .compare_exchange(0, word, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        while self.readers.load(Ordering::Acquire) != 0 {
            std::hint::spin_loop();
        }
        // SAFETY: as above; no copier can still take it.
        unsafe { PrototypeValidity::release_raw(word) };
        self.release_retired();
    }

    fn release_retired(&self) {
        let retired = self.retired.swap(0, Ordering::Relaxed);
        if retired != 0 {
            // SAFETY: a retired word owns its count; no copier is running.
            unsafe { PrototypeValidity::release_raw(retired) };
        }
    }

    /// A consistent copy of the live entries with each one's proof count
    /// taken, for a compiler thread; `None` when the mutator was rewriting
    /// them, which never waits for it.
    fn consistent_entries(&self) -> Option<Vec<(IcEntry, Option<Arc<PrototypeValidity>>)>> {
        self.readers.fetch_add(1, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::SeqCst);
        let result = (|| {
            let before = self.version.load(Ordering::Acquire);
            if before & 1 != 0 {
                return None;
            }
            let count = self.entry_count().min(PROFILED_PROPERTY_PIC_CAPACITY);
            let copies: Vec<IcEntry> = self.entries[..count].iter().map(IcEntry::copy).collect();
            std::sync::atomic::fence(Ordering::Acquire);
            if self.version.load(Ordering::Relaxed) != before {
                return None;
            }
            Some(
                copies
                    .into_iter()
                    .map(|entry| {
                        let word = entry.validity_word();
                        // SAFETY: the copy is consistent, and a word an
                        // entry drops while this runs is retired, not freed.
                        let cell =
                            (word != 0).then(|| unsafe { PrototypeValidity::clone_raw(word) });
                        (entry, cell)
                    })
                    .collect(),
            )
        })();
        self.readers.fetch_sub(1, Ordering::Release);
        result
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

    /// Bind an element site's cache to the one name key it serves, by atom:
    /// the first key binds it, the same key keeps it, and any other leaves
    /// the site megamorphic with no entries, which are keyed by shape alone.
    /// Whether the site still serves `atom` from its own entries. `index`
    /// is the integer index the atom spells, `None` for a symbol.
    pub(crate) fn bind_keyed_atom(&self, atom: u32, index: Option<u32>) -> bool {
        let state = self.state();
        if state & STATE_MEGAMORPHIC != 0 {
            return false;
        }
        if state & STATE_KEYED_BOUND == 0 {
            self.atom.store(atom, Ordering::Relaxed);
            self.keyed_index
                .store(index.unwrap_or(u32::MAX), Ordering::Relaxed);
            // Publishes the atom and index to a compiler thread's acquire.
            self.state
                .store(state | STATE_KEYED_BOUND, Ordering::Release);
            return true;
        }
        if self.atom.load(Ordering::Relaxed) == atom {
            return true;
        }
        self.begin_write();
        let dropped: smallvec::SmallVec<[u64; PROFILED_PROPERTY_PIC_CAPACITY]> =
            self.live().iter().map(IcEntry::clear).collect();
        self.state.store(
            (state & !STATE_COUNT_MASK) | STATE_MEGAMORPHIC,
            Ordering::Relaxed,
        );
        self.refresh_inline();
        self.end_write();
        for word in dropped {
            self.release_validity(word);
        }
        self.disables.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// The name key an element site's cache serves, while it serves one: its
    /// atom and the integer index it spells, `None` for a symbol.
    #[must_use]
    pub(crate) fn keyed_name(&self) -> Option<(u32, Option<u32>)> {
        let state = self.state.load(Ordering::Acquire);
        if state & STATE_KEYED_BOUND == 0 || state & STATE_MEGAMORPHIC != 0 {
            return None;
        }
        let atom = self.atom.load(Ordering::Relaxed);
        let index = self.keyed_index.load(Ordering::Relaxed);
        Some((atom, (index != u32::MAX).then_some(index)))
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

    /// Whether this `length` site has read something other than an Array.
    #[must_use]
    pub(crate) fn length_read_non_array(&self) -> bool {
        self.state() & STATE_LENGTH_NON_ARRAY != 0
    }

    /// Whether this `length` site has read something other than a typed
    /// array.
    #[must_use]
    pub(crate) fn length_read_non_typed_array(&self) -> bool {
        self.state() & STATE_LENGTH_NON_TYPED_ARRAY != 0
    }

    /// Whether this `length` site has read something other than a primitive
    /// string.
    #[must_use]
    pub(crate) fn length_read_non_string(&self) -> bool {
        self.state() & STATE_LENGTH_NON_STRING != 0
    }

    /// Mark a `length` read of a receiver that is not a primitive string.
    /// Returns whether this was the first.
    pub(crate) fn record_length_non_string(&self) -> bool {
        let state = self.state();
        if state & STATE_LENGTH_NON_STRING != 0 {
            return false;
        }
        self.state
            .store(state | STATE_LENGTH_NON_STRING, Ordering::Relaxed);
        true
    }

    /// Mark a `length` read of a receiver that is not a typed array.
    /// Returns whether this was the first.
    pub(crate) fn record_length_non_typed_array(&self) -> bool {
        let state = self.state();
        if state & STATE_LENGTH_NON_TYPED_ARRAY != 0 {
            return false;
        }
        self.state
            .store(state | STATE_LENGTH_NON_TYPED_ARRAY, Ordering::Relaxed);
        true
    }

    /// Mark a `length` read of a receiver that is not an Array. Returns
    /// whether this was the first.
    pub(crate) fn record_length_non_array(&self) -> bool {
        let state = self.state();
        if state & STATE_LENGTH_NON_ARRAY != 0 {
            return false;
        }
        self.state
            .store(state | STATE_LENGTH_NON_ARRAY, Ordering::Relaxed);
        true
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
        let entry = self.live().iter().find(|entry| {
            entry.kind() == own && entry.projection() == PropertyStorageProjection::Receiver
        });
        match entry {
            Some(entry) => {
                self.inline_field
                    .store(entry.field.load(Ordering::Relaxed), Ordering::Relaxed);
                self.inline_owner
                    .store(entry.owner.load(Ordering::Relaxed), Ordering::Relaxed);
                self.inline_shape.store(entry.shape(), Ordering::Relaxed);
            }
            None => {
                self.inline_shape.store(EMPTY_SHAPE, Ordering::Relaxed);
                self.inline_field.store(0, Ordering::Relaxed);
                self.inline_owner.store(0, Ordering::Relaxed);
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
        self.probe_load_hit(obj, heap).and_then(LoadHit::value)
    }

    /// Run the load handler of `obj`'s shape, including one that names an
    /// inherited getter for the caller to call. `None` on a miss.
    #[must_use]
    pub(crate) fn probe_load_hit(&self, obj: JsObject, heap: &otter_gc::GcHeap) -> Option<LoadHit> {
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
            IcHandlerKind::OwnAccessor => Some(true),
            IcHandlerKind::PrototypeField | IcHandlerKind::PrototypeAccessor => {
                entry.proof_holds().then_some(true)
            }
            IcHandlerKind::NonExistent => entry.proof_holds().then_some(false),
            _ => None,
        }
    }

    /// Run the load handler of a receiver whose named lookup starts at
    /// `start`. `None` on a miss.
    #[must_use]
    pub(crate) fn probe_projected_load(
        &self,
        start: JsObject,
        projection: PropertyStorageProjection,
        heap: &otter_gc::GcHeap,
    ) -> Option<LoadHit> {
        let shape = object::keyed_shape(start, heap);
        if shape.is_null() {
            return None;
        }
        self.probe_load_from(start, projection.encode_key(shape.offset()), heap)
    }

    /// Run the own-field store handler for the selected `bag` and projection.
    /// `false` on a miss; the original receiver never becomes this bag.
    #[must_use]
    pub(crate) fn probe_projected_store(
        &self,
        bag: JsObject,
        projection: PropertyStorageProjection,
        heap: &mut otter_gc::GcHeap,
        value: Value,
    ) -> bool {
        let shape = object::keyed_shape(bag, heap);
        if shape.is_null() {
            return false;
        }
        match self.matching(projection.encode_key(shape.offset())) {
            Some(entry) if entry.kind() == IcHandlerKind::StoreField => {
                object::store_proven_data_slot(bag, heap, entry.slot(), value);
                true
            }
            _ => false,
        }
    }

    /// Run the load handler keyed `key` with `obj` as lookup-start object.
    fn probe_load_from(&self, obj: JsObject, key: u32, heap: &otter_gc::GcHeap) -> Option<LoadHit> {
        let entry = self.matching(key)?;
        if entry.projection() == PropertyStorageProjection::ArrayExpando
            && entry.kind() != IcHandlerKind::OwnField
        {
            return None;
        }
        let holder = || match object::shape_body::prototype_of(entry.aux()) {
            object::shape_body::ShapePrototype::Object(holder) => Some(holder),
            _ => None,
        };
        match entry.kind() {
            IcHandlerKind::OwnField => Some(LoadHit::Value(object::load_proven_data_slot(
                obj,
                heap,
                entry.slot(),
            ))),
            IcHandlerKind::PrototypeField if entry.proof_holds() => Some(LoadHit::Value(
                object::load_proven_data_slot(holder()?, heap, entry.slot()),
            )),
            IcHandlerKind::OwnAccessor => {
                object::load_proven_getter(obj, heap, entry.slot()).map(LoadHit::Getter)
            }
            IcHandlerKind::PrototypeAccessor if entry.proof_holds() => {
                object::load_proven_getter(holder()?, heap, entry.slot()).map(LoadHit::Getter)
            }
            IcHandlerKind::NonExistent if entry.proof_holds() => {
                Some(LoadHit::Value(Value::undefined()))
            }
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

    /// Install `handler` into a non-global named IC.
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
            // Re-learning the handler an entry already holds (a receiver
            // that missed only because it reached the site before its shape
            // settled) is no transition: the site's feedback did not change.
            if entry.holds(&handler) {
                return InstallOutcome::Unchanged;
            }
            self.begin_write();
            let dropped = entry.write(handler);
            self.refresh_inline();
            self.end_write();
            self.release_validity(dropped);
            self.installs.fetch_add(1, Ordering::Relaxed);
            return InstallOutcome::Installed;
        }
        self.begin_write();
        let mut dropped = self.retain(|entry| entry.proof_holds());
        let count = self.entry_count();
        if count < PROFILED_PROPERTY_PIC_CAPACITY {
            dropped.push(self.entries[count].write(handler));
            self.set_count(count + 1);
            self.end_write();
            for word in dropped {
                self.release_validity(word);
            }
            self.installs.fetch_add(1, Ordering::Relaxed);
            return InstallOutcome::Installed;
        }
        dropped.extend(self.live().iter().map(IcEntry::clear));
        let state = (self.state() & !STATE_COUNT_MASK) | STATE_MEGAMORPHIC;
        self.state.store(state, Ordering::Relaxed);
        self.refresh_inline();
        self.end_write();
        for word in dropped {
            self.release_validity(word);
        }
        self.disables.fetch_add(1, Ordering::Relaxed);
        InstallOutcome::BecameMegamorphic
    }

    /// Keep only entries `keep` accepts, compacting in order, inside a
    /// write; the proof words of the dropped ones, whose counts the caller
    /// now owns.
    fn retain(
        &self,
        mut keep: impl FnMut(&IcEntry) -> bool,
    ) -> smallvec::SmallVec<[u64; PROFILED_PROPERTY_PIC_CAPACITY]> {
        let count = self.entry_count();
        let mut kept = 0;
        let mut dropped = smallvec::SmallVec::new();
        for index in 0..count {
            let entry = &self.entries[index];
            if !keep(entry) {
                dropped.push(entry.clear());
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
        dropped
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
        self.begin_write();
        let dropped = self.retain(|entry| {
            PropertyStorageProjection::decode_key(entry.shape())
                .is_some_and(|(_, shape)| live(shape))
                && (entry.aux.load(Ordering::Relaxed) == 0
                    || live(entry.aux.load(Ordering::Relaxed)))
                && (entry.owner.load(Ordering::Relaxed) == 0
                    || live(entry.owner.load(Ordering::Relaxed)))
        });
        self.end_write();
        for word in dropped {
            self.release_validity(word);
        }
    }

    /// The first live unprojected own-field load: `(receiver shape, logical slot)`.
    #[must_use]
    pub(crate) fn mono_own_field(&self) -> Option<(ShapeHandle, u16)> {
        let [entry] = self.live() else {
            return None;
        };
        (entry.kind() == IcHandlerKind::OwnField
            && entry.projection() == PropertyStorageProjection::Receiver)
            .then(|| (entry.shape_handle(), entry.slot()))
    }

    /// [`Self::mono_own_field`] from a consistent copy, for a compiler
    /// thread; `None` also while the mutator rewrites the entries.
    #[must_use]
    pub(crate) fn consistent_mono_own_field(&self) -> Option<(ShapeHandle, u16)> {
        let entries = self.consistent_entries()?;
        let [(entry, _)] = entries.as_slice() else {
            return None;
        };
        (entry.kind() == IcHandlerKind::OwnField
            && entry.projection() == PropertyStorageProjection::Receiver)
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
                    (IcHandlerKind::PrototypeAccessor, _) => IcEntryVariant::InheritedAccessor,
                    (IcHandlerKind::OwnAccessor, _) => IcEntryVariant::OwnAccessor,
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
                    storage_projection: entry.projection(),
                    storage_shape_id: object::shape_body::id_of(receiver).raw(),
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
    /// `proofs` names a shape in the compilation (and retains it) and
    /// retains a still-valid proof. An entry whose shape can
    /// no longer be named or whose proof died is dropped, as are entries with
    /// no CacheIR lowering (nonexistent keys, dictionary transitions): their
    /// receivers reach the optimized code like unseen shapes. `None` when
    /// nothing remains.
    pub(crate) fn jit_programs(
        &self,
        atom: u32,
        proofs: &mut impl crate::jit_roots::CompileProofs,
    ) -> Option<Vec<crate::jit::JitCacheIrProgram>> {
        use crate::jit::JitCacheIrOp;
        let entries = self.consistent_entries()?;
        let mut programs = Vec::with_capacity(entries.len());
        for (entry, cell) in &entries {
            // A lookup-start receiver lowers only as a function's bag owning
            // the data slot; an inherited or absent key, or another receiver
            // kind's start object, reaches the generic node like an unseen
            // shape.
            if entry.projection() == PropertyStorageProjection::LookupStart
                && entry.kind() != IcHandlerKind::OwnField
            {
                continue;
            }
            let receiver = entry.shape_handle();
            // A direct store never commits into a prototype, nor appends to
            // an object that cannot grow: both are immutable facts of the
            // receiver's shape, so a program for such a shape could never
            // pass. Exporting none lets an exact shape guard alone prove a
            // store's receiver eligible.
            let state = object::shape_body::state_of(receiver);
            match entry.kind() {
                IcHandlerKind::StoreField if state.is_prototype() => continue,
                IcHandlerKind::StoreTransition
                    if state.is_prototype() || !state.is_extensible() =>
                {
                    continue;
                }
                _ => {}
            }
            let Some(shape) = proofs.shape(receiver) else {
                continue;
            };
            let validity = match cell {
                None => None,
                Some(cell) => match proofs.validity(cell) {
                    Some(validity) => Some(validity),
                    None => continue,
                },
            };
            let field = entry.field();
            let mut ops: Vec<JitCacheIrOp> = match entry.kind() {
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
                    let (Some(root), Some(validity)) = (proofs.shape(entry.aux()), validity) else {
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
                    let target = entry.aux();
                    if !object::shape_body::append_geometry_matches(receiver, target, field) {
                        continue;
                    }
                    let Some(to_shape) = proofs.shape(target) else {
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
                IcHandlerKind::NonExistent
                | IcHandlerKind::PrototypeAccessor
                | IcHandlerKind::OwnAccessor
                | IcHandlerKind::Empty => continue,
            };
            if entry.projection() == PropertyStorageProjection::LookupStart {
                for op in &mut ops {
                    match op {
                        JitCacheIrOp::GuardShape { object, .. }
                        | JitCacheIrOp::GuardAtomSlot { object, .. }
                        | JitCacheIrOp::LoadField { object, .. } => *object = 1,
                        _ => unreachable!("a function bag lowers existing own slots only"),
                    }
                }
                ops.insert(
                    0,
                    JitCacheIrOp::LoadFunctionBag {
                        object: 0,
                        result: 1,
                    },
                );
            }
            if entry.projection() == PropertyStorageProjection::TypedArrayStart {
                // The receiver proof reads the start into operand 1; the
                // start's own or inherited slot follows as for an object.
                for op in &mut ops {
                    match op {
                        JitCacheIrOp::GuardShape { object, .. }
                        | JitCacheIrOp::GuardAtomSlot { object, .. }
                        | JitCacheIrOp::LoadField { object, .. }
                            if *object == 0 =>
                        {
                            *object = 1;
                        }
                        JitCacheIrOp::GuardShape { .. }
                        | JitCacheIrOp::GuardAtomSlot { .. }
                        | JitCacheIrOp::LoadField { .. }
                        | JitCacheIrOp::GuardPrototypeValidity { .. }
                        | JitCacheIrOp::LoadPrototypeHolder { .. } => {}
                        _ => unreachable!("a typed array's start lowers loaded slots only"),
                    }
                }
                ops.insert(
                    0,
                    JitCacheIrOp::LoadTypedArrayStart {
                        object: 0,
                        result: 1,
                    },
                );
            }
            if entry.projection() == PropertyStorageProjection::ArrayExpando {
                if !matches!(
                    entry.kind(),
                    IcHandlerKind::OwnField | IcHandlerKind::StoreField
                ) {
                    continue;
                }
                for op in &mut ops {
                    match op {
                        JitCacheIrOp::GuardShape { object, .. }
                        | JitCacheIrOp::GuardAtomSlot { object, .. }
                        | JitCacheIrOp::LoadField { object, .. }
                        | JitCacheIrOp::StoreField { object, .. } => *object = 1,
                        _ => unreachable!("array expando admits existing own slots only"),
                    }
                }
                ops.insert(
                    0,
                    JitCacheIrOp::LoadArrayExpando {
                        object: 0,
                        result: 1,
                        layout: crate::jit::JitArrayLayout::current(),
                    },
                );
            }
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
            let word = entry.clear();
            if word != 0 {
                // SAFETY: the word owned its count; nothing reads a dying slot.
                unsafe { PrototypeValidity::release_raw(word) };
            }
        }
        self.release_retired();
    }
}

const _: () = {
    assert!(std::mem::size_of::<IcEntry>() == 32);
    assert!(std::mem::offset_of!(IcEntry, validity) == 24);
    assert!(std::mem::offset_of!(PropertyIcSlot, inline_shape) == 0);
    assert!(std::mem::offset_of!(PropertyIcSlot, inline_field) == 4);
    assert!(std::mem::offset_of!(PropertyIcSlot, entries) == 32);
};

#[cfg(test)]
#[path = "property_ic/tests.rs"]
mod tests;
