//! Runtime iterator-state machine.
//!
//! Each `Op::GetIterator` produces an [`IteratorHandle`] pointing at
//! one of these states; every `Op::IteratorNext` advances the state
//! by one step. Built-in iterators (Array / String / Map / Set /
//! RegExp-String) plus the iterator-helpers proposal wrappers (map /
//! filter / take / drop / flatMap) share this enum so the dispatcher
//! can drive every shape with one opcode.
//!
//! # Contents
//! - [`IteratorState`] — variant enum, one variant per iterator
//!   shape.
//! - [`ArrayIterKind`] / [`MapIteratorKind`] / [`SetIteratorKind`] —
//!   yield-shape discriminators for the Array/Map/Set methods.
//! - [`BuiltinIteratorOrigin`] — prototype-routing tag used by
//!   `[[GetPrototypeOf]]` so each kind exposes its spec-mandated
//!   `@@toStringTag`.
//! - [`ITERATOR_STATE_TYPE_TAG`] — GC body type tag (distinct from
//!   `BOUND_FUNCTION_BODY_TYPE_TAG` so the eight-byte tagged
//!   [`crate::Value`] family-dispatch can disambiguate them).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-iterator-objects>
//! - <https://tc39.es/proposal-iterator-helpers/>

use crate::Value;
use crate::array::JsArray;
use crate::binary::typed_array::JsTypedArray;
use crate::collections::{JsMap, JsSet};
use crate::string::JsString;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`IteratorState`].
///
/// Previously shared `0x1c` with
/// [`crate::BOUND_FUNCTION_BODY_TYPE_TAG`]. The collision is fatal
/// once the eight-byte tagged [`crate::value::Value`] dispatches
/// through [`otter_gc::raw::RawGc::checked_cast`] — both bodies live
/// in the `TAG_PTR_FUNCTION` family, and a shared tag would let an
/// iterator handle masquerade as a bound function. Bumped to a fresh
/// value here.
pub const ITERATOR_STATE_TYPE_TAG: u8 = 0x24;

/// Heap-shared iterator state handle.
pub type IteratorHandle = otter_gc::Gc<IteratorState>;

/// Kind discriminator for `Array.prototype.{values, keys, entries}`
/// iterators, matching `CreateArrayIterator(O, kind)` per §23.1.5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayIterKind {
    /// `values()` — yields each element.
    Value,
    /// `keys()` — yields the numeric index.
    Key,
    /// `entries()` — yields a fresh `[index, value]` Array.
    Entry,
}

/// Kind discriminator for `Map.prototype.{keys, values, entries}`
/// iterators, matching `CreateMapIterator(map, kind)` per §24.1.5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapIteratorKind {
    /// `keys()` — yields each map key.
    Key,
    /// `values()` — yields each map value.
    Value,
    /// `entries()` / `@@iterator` — yields `[key, value]` Arrays.
    Entry,
}

/// Kind discriminator for `Set.prototype.{values, entries}` iterators,
/// matching `CreateSetIterator(set, kind)` per §24.2.5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetIteratorKind {
    /// `values()` / `keys()` / `@@iterator` — yields each set value.
    Value,
    /// `entries()` — yields `[value, value]` Arrays.
    Entry,
}

/// Origin of a built-in iterator. Used to route `[[GetPrototypeOf]]`
/// to the correct per-kind iterator prototype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BuiltinIteratorOrigin {
    /// `%ArrayIteratorPrototype%`.
    #[default]
    Array,
    /// `%MapIteratorPrototype%`.
    Map,
    /// `%SetIteratorPrototype%`.
    Set,
    /// `%StringIteratorPrototype%`.
    String,
    /// `%RegExpStringIteratorPrototype%`.
    RegExpString,
    /// `%IteratorHelperPrototype%` — §27.1.2.1.
    Helper,
    /// `%WrapForValidIteratorPrototype%` — §27.1.3.2.
    WrapForValidIterator,
}

/// Runtime state for iterator handles driven via `Op::IteratorNext`.
/// Covers both built-in array / string iterators and the lazy
/// iterator-helpers wrappers per the proposal.
#[derive(Debug, otter_macros::Pelt)]
#[pelt(tag = ITERATOR_STATE_TYPE_TAG)]
pub enum IteratorState {
    /// Walks `array`'s dense storage in insertion order.
    Array {
        /// Backing array — held by `JsArray`'s GC handle so mutation
        /// through the original handle is observable.
        array: JsArray,
        /// Next element index to read. Compared against the array's
        /// `len()` at every step so resizing the array during
        /// iteration is observed correctly.
        #[pelt(skip)]
        index: usize,
        /// Per-kind iterator origin. Map / Set / String iterators
        /// reuse the dense-array snapshot shape but inherit from
        /// distinct realm prototypes (§23.1.5 / §24.1.5 / §24.2.5 /
        /// §22.1.5) carrying their kind-specific `@@toStringTag`.
        #[pelt(skip)]
        origin: BuiltinIteratorOrigin,
    },
    /// Walks `array` yielding only its numeric indices —
    /// `Array.prototype.keys()` per §23.1.3.18.
    ArrayKey {
        /// Backing array.
        array: JsArray,
        /// Next index to yield.
        #[pelt(skip)]
        index: usize,
    },
    /// Walks `array` yielding `[index, value]` pairs per §23.1.3.7
    /// `Array.prototype.entries()`.
    ArrayEntry {
        /// Backing array.
        array: JsArray,
        /// Next index to yield.
        #[pelt(skip)]
        index: usize,
    },
    /// Live walk over a generic array-like object (e.g. an `arguments`
    /// object) per §23.1.5.1 `CreateArrayIterator(O, kind)`. Reads
    /// `LengthOfArrayLike(O)` and `Get(O, index)` on every step so a
    /// `length` / element mutation during iteration is observed —
    /// unlike the dense `Array` states, whose backing handle already
    /// reflects mutation, this holds an arbitrary object.
    ArrayLike {
        /// Backing array-like object (traced live).
        object: Value,
        /// Next index to read.
        #[pelt(skip)]
        index: usize,
        /// Yield shape (values / keys / entries).
        #[pelt(skip)]
        kind: ArrayIterKind,
    },
    /// Live walk over a TypedArray's elements per §23.2.5.1
    /// `CreateArrayIterator(O, kind)`. Unlike the Array snapshot
    /// states, this reads `typed_array[index]` on every step so
    /// element mutations during iteration are observed, and reports
    /// `done` when the backing buffer is detached.
    TypedArray {
        /// Backing typed array (traced live).
        #[pelt(via = crate::binary::typed_array::JsTypedArray::trace_value_slots)]
        typed_array: JsTypedArray,
        /// Next element index to read.
        #[pelt(skip)]
        index: usize,
        /// Yield shape (values / keys / entries).
        #[pelt(skip)]
        kind: ArrayIterKind,
    },
    /// Walks `string`'s WTF-16 code units while yielding full
    /// code-point strings; surrogate pairs advance as one item.
    String {
        /// Backing string.
        #[pelt(skip)]
        string: JsString,
        /// Next code-unit index.
        #[pelt(skip)]
        index: u32,
    },
    /// Lazy RegExp String Iterator created by
    /// `RegExp.prototype[@@matchAll]` per §22.2.7.2.
    RegExpString {
        /// The cloned matcher object used for iteration.
        matcher: Value,
        /// Input string being matched.
        #[pelt(skip)]
        input: JsString,
        /// Whether the matcher has the `g` flag.
        #[pelt(skip)]
        global: bool,
        /// Whether `AdvanceStringIndex` uses Unicode mode (`u`/`v`).
        #[pelt(skip)]
        full_unicode: bool,
        /// Sticky exhaustion flag for repeated `done` results.
        #[pelt(skip)]
        done: bool,
    },
    /// Walks a live `Map` in insertion order.
    MapCollection {
        /// Backing map.
        map: JsMap,
        /// Next live entry index.
        #[pelt(skip)]
        index: usize,
        /// Yield shape.
        #[pelt(skip)]
        kind: MapIteratorKind,
    },
    /// Walks a live `Set` in insertion order.
    SetCollection {
        /// Backing set.
        set: JsSet,
        /// Next live entry index.
        #[pelt(skip)]
        index: usize,
        /// Yield shape.
        #[pelt(skip)]
        kind: SetIteratorKind,
    },
    /// User-defined iterable: the result of calling `obj[@@iterator]()`.
    User {
        /// Iterator object returned by `obj[@@iterator]()`.
        iterator: Value,
        /// §7.4.4 GetIteratorDirect — `next` read once when the
        /// iterator record is created (Iterator.from / iterator
        /// helpers). `None` for paths that defer the lookup to the
        /// first step (for-of `GetIterator`).
        next_method: Option<Value>,
    },
    /// Permanently exhausted iterator. Keeps the origin of the state
    /// it replaced so `[[GetPrototypeOf]]` stays stable across
    /// exhaustion.
    Exhausted {
        /// Prototype-routing origin of the pre-exhaustion state.
        #[pelt(skip)]
        origin: Option<BuiltinIteratorOrigin>,
    },
    /// Lazy `Iterator.prototype.map(fn)` wrapper.
    Map {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Per-element mapper. Must be callable.
        mapper: Value,
        /// True while the mapper callback is running. Re-entrant
        /// `.next()` calls on the same helper must throw without
        /// closing the underlying iterator.
        #[pelt(skip)]
        running: bool,
        /// §27.1.4.7 — zero-based counter passed as the mapper's
        /// second argument.
        #[pelt(skip)]
        counter: u64,
    },
    /// Lazy `Iterator.prototype.filter(predicate)` wrapper.
    Filter {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Per-element predicate. Must be callable.
        predicate: Value,
        /// True while the predicate callback is running.
        #[pelt(skip)]
        running: bool,
        /// §27.1.4.6 — zero-based counter passed as the predicate's
        /// second argument.
        #[pelt(skip)]
        counter: u64,
    },
    /// Lazy `Iterator.prototype.take(n)` wrapper.
    Take {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Steps still allowed before the wrapper reports `done`.
        #[pelt(skip)]
        remaining: u64,
    },
    /// Lazy `Iterator.prototype.drop(n)` wrapper.
    Drop {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Elements still to discard before forwarding kicks in.
        #[pelt(skip)]
        to_drop: u64,
    },
    /// `Value::Generator` driven through the iterator protocol.
    Generator {
        /// Underlying generator handle.
        #[pelt(via = crate::generator::JsGenerator::trace_value_slots)]
        handle: crate::generator::JsGenerator,
    },
    /// Lazy `Iterator.prototype.flatMap(mapper)` wrapper.
    FlatMap {
        /// Underlying iterator handle.
        source: IteratorHandle,
        /// Per-element mapper. Must be callable.
        mapper: Value,
        /// True while the mapper callback is running.
        #[pelt(skip)]
        running: bool,
        /// Inner iterator currently being drained, when the last
        /// `mapper` call produced an iterable.
        inner: Option<IteratorHandle>,
        /// §27.1.4.5 — zero-based counter passed as the mapper's
        /// second argument.
        #[pelt(skip)]
        counter: u64,
    },
}

impl IteratorState {
    /// Per-kind iterator-prototype origin for the built-in iterator
    /// variants. Returns `None` for variants whose prototype is
    /// `%IteratorPrototype%` directly (user iterators, helpers,
    /// generators).
    #[must_use]
    pub fn builtin_origin(&self) -> Option<BuiltinIteratorOrigin> {
        match self {
            IteratorState::Array { origin, .. } => Some(*origin),
            IteratorState::ArrayKey { .. }
            | IteratorState::ArrayEntry { .. }
            | IteratorState::ArrayLike { .. }
            | IteratorState::TypedArray { .. } => {
                // §23.2.5.1 — TypedArray iterators inherit
                // %ArrayIteratorPrototype% just like Array iterators.
                Some(BuiltinIteratorOrigin::Array)
            }
            IteratorState::String { .. } => Some(BuiltinIteratorOrigin::String),
            IteratorState::RegExpString { .. } => Some(BuiltinIteratorOrigin::RegExpString),
            IteratorState::MapCollection { .. } => Some(BuiltinIteratorOrigin::Map),
            IteratorState::SetCollection { .. } => Some(BuiltinIteratorOrigin::Set),
            // §27.1.2.1 — the lazy helper wrappers expose
            // `%IteratorHelperPrototype%` (own `next` / `return`).
            IteratorState::Map { .. }
            | IteratorState::Filter { .. }
            | IteratorState::Take { .. }
            | IteratorState::Drop { .. }
            | IteratorState::FlatMap { .. } => Some(BuiltinIteratorOrigin::Helper),
            // §27.1.3.2 — wrapped user / generator iterators expose
            // `%WrapForValidIteratorPrototype%` (own `next` / `return`).
            IteratorState::User { .. } | IteratorState::Generator { .. } => {
                Some(BuiltinIteratorOrigin::WrapForValidIterator)
            }
            IteratorState::Exhausted { origin } => *origin,
        }
    }

    /// Fold the state to [`IteratorState::Exhausted`] while keeping
    /// the prototype-routing origin stable.
    pub fn exhaust(&mut self) {
        *self = IteratorState::Exhausted {
            origin: self.builtin_origin(),
        };
    }
}
