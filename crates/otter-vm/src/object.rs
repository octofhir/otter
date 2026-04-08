//! Minimal object heap and inline-cache support for the new VM.

use std::collections::BTreeMap;

use otter_gc::heap::GcConfig;
use otter_gc::typed::{Handle as GcHandle, OutOfMemory, Traceable, TypedHeap};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Maximum JavaScript array length, per ECMA-262 §7.1.22
/// (<https://tc39.es/ecma262/#sec-tolength>) and §22.1.3.1
/// (<https://tc39.es/ecma262/#sec-array.prototype.concat>). A valid array
/// length is a uint32 value `<= 2^32 - 1`.
pub const MAX_ARRAY_LENGTH: usize = u32::MAX as usize;

use crate::bytecode::ProgramCounter;
use crate::host::HostFunctionId;
use crate::js_string::JsString;
use crate::module::FunctionIndex;
use crate::module::Module;
use crate::payload::NativePayloadId;
use crate::property::{PropertyNameId, PropertyNameRegistry};
use crate::value::RegisterValue;

/// §6.2.12 Private Name — uniquely identifies a private class element.
/// Each class evaluation produces a unique `class_id`; combined with the
/// element description it forms a globally unique key for `[[PrivateElements]]`.
/// Spec: <https://tc39.es/ecma262/#sec-private-names>
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateNameKey {
    /// Unique identifier per class evaluation (monotonic counter).
    pub class_id: u64,
    /// The description string without the `#` prefix (e.g. `"x"` for `#x`).
    pub description: Box<str>,
}

/// §6.2.10 PrivateElement — a private field, method, or accessor stored
/// in an object's `[[PrivateElements]]` internal slot.
/// Spec: <https://tc39.es/ecma262/#sec-privateelement-specification-type>
#[derive(Debug, Clone, PartialEq)]
pub enum PrivateElement {
    /// A private instance or static field value.
    Field(RegisterValue),
    /// A private method (not writable via PrivateSet).
    Method(ObjectHandle),
    /// A private accessor pair.
    Accessor {
        getter: Option<ObjectHandle>,
        setter: Option<ObjectHandle>,
    },
}

/// Maximum prototype chain depth before aborting a lookup (defense in depth).
const MAX_PROTOTYPE_DEPTH: usize = 45;

/// Maximum prototype chain depth for cycle detection in `set_prototype`.
const MAX_SET_PROTOTYPE_DEPTH: usize = 100;

/// Stable object handle encoded in register values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectHandle(pub u32);

/// Stable shape identifier for object inline caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectShapeId(pub u64);

/// Monomorphic inline-cache entry for a named property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PropertyInlineCache {
    shape_id: ObjectShapeId,
    slot_index: u16,
}

impl PropertyInlineCache {
    /// Creates a monomorphic property cache entry.
    #[must_use]
    pub const fn new(shape_id: ObjectShapeId, slot_index: u16) -> Self {
        Self {
            shape_id,
            slot_index,
        }
    }

    /// Returns the cached shape identifier.
    #[must_use]
    pub const fn shape_id(self) -> ObjectShapeId {
        self.shape_id
    }

    /// Returns the cached property slot index.
    #[must_use]
    pub const fn slot_index(self) -> u16 {
        self.slot_index
    }
}

/// Result of an ordinary named-property lookup.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropertyLookup {
    owner: ObjectHandle,
    value: PropertyValue,
    cache: Option<PropertyInlineCache>,
}

impl PropertyLookup {
    #[must_use]
    pub const fn new(
        owner: ObjectHandle,
        value: PropertyValue,
        cache: Option<PropertyInlineCache>,
    ) -> Self {
        Self {
            owner,
            value,
            cache,
        }
    }

    #[must_use]
    pub const fn owner(self) -> ObjectHandle {
        self.owner
    }

    #[must_use]
    pub const fn value(self) -> PropertyValue {
        self.value
    }

    #[must_use]
    pub const fn cache(self) -> Option<PropertyInlineCache> {
        self.cache
    }
}

/// Outcome of the inner `set_array_length_inner` mutation, reported back
/// to the outer `set_array_length` so it can reconcile the heap budget
/// without holding a mutable borrow into the object.
#[derive(Debug, Clone, Copy)]
struct SetLengthOutcome {
    /// Whether the spec-level result is `true` (success) or `false`
    /// (no-op due to `!extensible` / `!configurable` etc.).
    ok: bool,
    /// True if the Vec was actually grown — caller should keep the byte
    /// reservation. False means the caller should release it.
    grew: bool,
    /// Number of bytes freed by a truncation. Caller releases these from
    /// the heap budget after the mutable borrow ends.
    shrunk_bytes: usize,
}

/// Per-type heap statistics returned by [`ObjectHeap::collect_type_stats`].
/// Used by the memory-leak profiler in the test262 runner to detect growth
/// between tests.
#[derive(Debug, Clone, Default)]
pub struct HeapTypeStats {
    /// Total number of live objects across all variants.
    pub total_count: usize,
    /// Tracked memory footprint across all variants, in bytes. Includes
    /// `size_of::<HeapValue>()` for every slot plus estimated internals
    /// (Array elements, ArrayBuffer bytes, Object value slots).
    pub total_bytes: usize,
    /// Per-variant count + bytes.
    pub by_type: std::collections::BTreeMap<&'static str, (usize, usize)>,
}

/// Error produced by the minimal object heap.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ObjectError {
    /// The object handle does not exist in the current heap.
    InvalidHandle,
    /// The heap value exists, but the requested operation is not supported.
    InvalidKind,
    /// The heap value exists, but the requested slot index is out of bounds.
    InvalidIndex,
    /// The requested array length is not a valid ECMAScript array length.
    InvalidArrayLength,
    /// The operation would exceed the configured heap cap. Set when a
    /// container growth (e.g. `Array.elements.resize`) overshoots the
    /// reservation budget via `TypedHeap::reserve_bytes`.
    OutOfMemory,
    /// A TypeError-level semantic violation (e.g. private name access).
    TypeError(Box<str>),
}

impl From<OutOfMemory> for ObjectError {
    fn from(_: OutOfMemory) -> Self {
        ObjectError::OutOfMemory
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeapValueKind {
    /// Plain object with named properties.
    Object,
    /// Host-callable native function object.
    HostFunction,
    /// Dense array with indexed elements.
    Array,
    /// String storage with indexed character access.
    String,
    /// Closure object with captured upvalue cells.
    Closure,
    /// Bound function exotic object (§10.4.1).
    BoundFunction,
    /// Mutable cell used to back one captured upvalue.
    UpvalueCell,
    /// Internal iterator used by the new VM iteration lowering.
    Iterator,
    /// ES2024 Promise object.
    Promise,
    /// ES2024 Map object.
    Map,
    /// ES2024 Set object.
    Set,
    /// ES2024 Map iterator.
    MapIterator,
    /// ES2024 Set iterator.
    SetIterator,
    /// ES2024 §24.3 WeakMap object.
    WeakMap,
    /// ES2024 §24.4 WeakSet object.
    WeakSet,
    /// ES2024 §26.1 WeakRef object.
    WeakRef,
    /// ES2024 §26.2 FinalizationRegistry object.
    FinalizationRegistry,
    /// ES2024 §27.5 Generator object.
    Generator,
    /// ES2024 §27.6 AsyncGenerator object.
    AsyncGenerator,
    /// ES2024 §25.1 ArrayBuffer object.
    ArrayBuffer,
    /// ES2024 §25.2 SharedArrayBuffer object.
    SharedArrayBuffer,
    /// ES2024 §22.2 RegExp object.
    RegExp,
    /// ES2024 §27.2.1.5 — Promise capability resolve/reject function.
    PromiseCapabilityFunction,
    /// Promise combinator per-element function (all/allSettled/any).
    PromiseCombinatorElement,
    /// Promise.prototype.finally wrapper function.
    PromiseFinallyFunction,
    /// Value thunk for finally (returns or throws a captured value).
    PromiseValueThunk,
    /// ES2024 §28.2 Proxy exotic object.
    Proxy,
    /// ES2024 §23.2 TypedArray object (Int8Array, Uint8Array, etc.).
    TypedArray,
    /// ES2024 §25.3 DataView object.
    DataView,
    /// ES2024 §6.1.6.2 BigInt primitive (heap-allocated).
    BigInt,
    /// V8-extension stack frame snapshot bag, used to back the lazily
    /// formatted `Error.prototype.stack` accessor.
    ErrorStackFrames,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IteratorStep {
    done: bool,
    value: RegisterValue,
}

impl IteratorStep {
    #[must_use]
    pub const fn done() -> Self {
        Self {
            done: true,
            value: RegisterValue::undefined(),
        }
    }

    #[must_use]
    pub const fn yield_value(value: RegisterValue) -> Self {
        Self { done: false, value }
    }

    #[must_use]
    pub const fn is_done(self) -> bool {
        self.done
    }

    #[must_use]
    pub const fn value(self) -> RegisterValue {
        self.value
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IteratorCursor {
    iterable: ObjectHandle,
    next_index: usize,
    closed: bool,
    is_array: bool,
}

impl IteratorCursor {
    #[must_use]
    pub const fn iterable(self) -> ObjectHandle {
        self.iterable
    }

    #[must_use]
    pub const fn next_index(self) -> usize {
        self.next_index
    }

    #[must_use]
    pub const fn closed(self) -> bool {
        self.closed
    }

    #[must_use]
    pub const fn is_array(self) -> bool {
        self.is_array
    }
}

/// ES2024 §23.1.5.1 — The kind of values an Array iterator yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayIteratorKind {
    /// `Array.prototype.keys()` — yields indices.
    Keys,
    /// `Array.prototype.values()` / `Array.prototype[@@iterator]()` — yields values.
    Values,
    /// `Array.prototype.entries()` — yields `[index, value]` pairs.
    Entries,
}

/// ES2024 §24.1.5.1 — The kind of values a Map iterator yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MapIteratorKind {
    /// `Map.prototype.keys()` — yields keys.
    Keys,
    /// `Map.prototype.values()` — yields values.
    Values,
    /// `Map.prototype.entries()` / `Map.prototype[@@iterator]()` — yields `[key, value]` pairs.
    Entries,
}

/// ES2024 §24.2.5.1 — The kind of values a Set iterator yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetIteratorKind {
    /// `Set.prototype.values()` / `Set.prototype[@@iterator]()` / `Set.prototype.keys()`.
    Values,
    /// `Set.prototype.entries()` — yields `[value, value]` pairs.
    Entries,
}

/// ES2024 §26.2.1.2 — FinalizationRegistry cell record.
/// Spec: <https://tc39.es/ecma262/#sec-weak-ref-processing-model>
#[derive(Debug, Clone, PartialEq)]
pub struct FinalizationCell {
    /// The weakly-held target (handle index). `None` = already collected.
    pub target: Option<u32>,
    /// The value held for the cleanup callback.
    pub held_value: RegisterValue,
    /// Optional unregister token (handle index). `None` = no token.
    pub unregister_token: Option<u32>,
}

/// ES2024 §23.2 — TypedArray element type discriminator.
///
/// Each variant corresponds to a concrete TypedArray constructor.
/// Spec: <https://tc39.es/ecma262/#table-the-typedarray-constructors>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypedArrayKind {
    /// Int8Array — 1 byte, signed.
    Int8,
    /// Uint8Array — 1 byte, unsigned.
    Uint8,
    /// Uint8ClampedArray — 1 byte, unsigned, clamped conversion.
    Uint8Clamped,
    /// Int16Array — 2 bytes, signed.
    Int16,
    /// Uint16Array — 2 bytes, unsigned.
    Uint16,
    /// Int32Array — 4 bytes, signed.
    Int32,
    /// Uint32Array — 4 bytes, unsigned.
    Uint32,
    /// Float32Array — 4 bytes, IEEE 754 single.
    Float32,
    /// Float64Array — 8 bytes, IEEE 754 double.
    Float64,
    /// BigInt64Array — 8 bytes, signed bigint.
    BigInt64,
    /// BigUint64Array — 8 bytes, unsigned bigint.
    BigUint64,
}

impl TypedArrayKind {
    /// Returns the byte size of a single element.
    /// §23.2.6 Table 70 — TypedArray Element Sizes.
    #[must_use]
    pub const fn element_size(self) -> usize {
        match self {
            Self::Int8 | Self::Uint8 | Self::Uint8Clamped => 1,
            Self::Int16 | Self::Uint16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float32 => 4,
            Self::Float64 | Self::BigInt64 | Self::BigUint64 => 8,
        }
    }

    /// Returns the ECMAScript constructor name.
    #[must_use]
    pub const fn constructor_name(self) -> &'static str {
        match self {
            Self::Int8 => "Int8Array",
            Self::Uint8 => "Uint8Array",
            Self::Uint8Clamped => "Uint8ClampedArray",
            Self::Int16 => "Int16Array",
            Self::Uint16 => "Uint16Array",
            Self::Int32 => "Int32Array",
            Self::Uint32 => "Uint32Array",
            Self::Float32 => "Float32Array",
            Self::Float64 => "Float64Array",
            Self::BigInt64 => "BigInt64Array",
            Self::BigUint64 => "BigUint64Array",
        }
    }

    /// Returns whether this kind is a BigInt typed array (BigInt64Array or BigUint64Array).
    /// §23.2.6 Table 70 — BigInt typed arrays have [[ContentType]] = BigInt.
    #[must_use]
    pub const fn is_bigint_kind(self) -> bool {
        matches!(self, Self::BigInt64 | Self::BigUint64)
    }

    /// Returns all TypedArray kinds for iteration.
    #[must_use]
    pub const fn all() -> &'static [TypedArrayKind] {
        &[
            Self::Int8,
            Self::Uint8,
            Self::Uint8Clamped,
            Self::Int16,
            Self::Uint16,
            Self::Int32,
            Self::Uint32,
            Self::Float32,
            Self::Float64,
            Self::BigInt64,
            Self::BigUint64,
        ]
    }
}

/// ES2024 §27.5.3 — Generator state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeneratorState {
    /// Created but body not yet started executing.
    SuspendedStart,
    /// Suspended at a `yield` point.
    SuspendedYield,
    /// Currently executing (inside a `.next()` call).
    Executing,
    /// Completed (returned or threw).
    Completed,
    /// §27.6 — Awaiting return (async generator only, waiting for AwaitReturn promise).
    AwaitingReturn,
}

/// ES2024 §27.6.3.1 — AsyncGeneratorRequest record.
/// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorrequest-records>
///
/// Each `.next(v)` / `.return(v)` / `.throw(v)` call on an async generator
/// pushes one request into the queue. The generator processes them in FIFO order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsyncGeneratorRequest {
    /// The completion kind that initiated this request.
    pub kind: AsyncGeneratorRequestKind,
    /// The value passed to `.next(v)` / `.return(v)` / `.throw(v)`.
    pub value: RegisterValue,
    /// The promise to be resolved/rejected when this request completes.
    pub promise: ObjectHandle,
}

/// The kind of operation that created an async generator request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncGeneratorRequestKind {
    /// `.next(value)`
    Next,
    /// `.return(value)`
    Return,
    /// `.throw(value)`
    Throw,
}

/// ES2024 §10.2 — Closure function kind flags.
///
/// Packed `u8` bitfield encoding whether a closure is an arrow function,
/// method definition, generator, or async function. These flags determine
/// `[[Construct]]` eligibility (§7.2.4 IsConstructor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClosureFlags(u8);

const FLAG_ARROW: u8 = 0x01;
const FLAG_METHOD: u8 = 0x02;
const FLAG_GENERATOR: u8 = 0x04;
const FLAG_ASYNC: u8 = 0x08;
const FLAG_CLASS_CONSTRUCTOR: u8 = 0x10;

impl ClosureFlags {
    /// Regular function declaration/expression — has `[[Construct]]`.
    #[must_use]
    pub const fn normal() -> Self {
        Self(0)
    }

    /// Arrow function (§15.3) — no `[[Construct]]`, no own `this`.
    #[must_use]
    pub const fn arrow() -> Self {
        Self(FLAG_ARROW)
    }

    /// Method definition (§15.4.4) — no `[[Construct]]`.
    #[must_use]
    pub const fn method() -> Self {
        Self(FLAG_METHOD)
    }

    /// Generator function — no `[[Construct]]` (§15.5.1).
    #[must_use]
    pub const fn generator() -> Self {
        Self(FLAG_GENERATOR)
    }

    /// Async function.
    #[must_use]
    pub const fn async_fn() -> Self {
        Self(FLAG_ASYNC)
    }

    /// Async generator function — `async function*`.
    /// §27.6 — no `[[Construct]]`, generator + async.
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-objects>
    #[must_use]
    pub const fn async_generator() -> Self {
        Self(FLAG_GENERATOR | FLAG_ASYNC)
    }

    /// Async arrow function — no `[[Construct]]`, no own `this`, async body.
    #[must_use]
    pub const fn async_arrow() -> Self {
        Self(FLAG_ASYNC | FLAG_ARROW)
    }

    /// Class constructor (§15.7) — constructable, but plain calls throw.
    #[must_use]
    pub const fn class_constructor() -> Self {
        Self(FLAG_CLASS_CONSTRUCTOR)
    }

    /// ES2024 §7.2.4 IsConstructor — true only for regular function declarations/expressions.
    #[must_use]
    pub const fn is_constructable(self) -> bool {
        self.0 == 0 || self.0 == FLAG_CLASS_CONSTRUCTOR
    }

    #[must_use]
    pub const fn is_arrow(self) -> bool {
        self.0 & FLAG_ARROW != 0
    }

    #[must_use]
    pub const fn is_method(self) -> bool {
        self.0 & FLAG_METHOD != 0
    }

    #[must_use]
    pub const fn is_generator(self) -> bool {
        self.0 & FLAG_GENERATOR != 0
    }

    #[must_use]
    pub const fn is_async(self) -> bool {
        self.0 & FLAG_ASYNC != 0
    }

    #[must_use]
    pub const fn is_class_constructor(self) -> bool {
        self.0 & FLAG_CLASS_CONSTRUCTOR != 0
    }
}

impl Default for ClosureFlags {
    fn default() -> Self {
        Self::normal()
    }
}

/// ES2024 §6.1.7.1 — Property Attributes.
///
/// Packed `u8` bitfield: bit 0 = writable, bit 1 = enumerable, bit 2 = configurable.
/// Factory methods encode the attribute combinations mandated by the spec for
/// different property origins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PropertyAttributes(u8);

const ATTR_WRITABLE: u8 = 0x01;
const ATTR_ENUMERABLE: u8 = 0x02;
const ATTR_CONFIGURABLE: u8 = 0x04;

impl PropertyAttributes {
    /// User-assigned data properties (§10.1.9 OrdinarySet step 3.d).
    /// { [[Writable]]: true, [[Enumerable]]: true, [[Configurable]]: true }
    #[must_use]
    pub const fn data() -> Self {
        Self(ATTR_WRITABLE | ATTR_ENUMERABLE | ATTR_CONFIGURABLE)
    }

    /// Built-in prototype methods (§18 ECMAScript Standard Built-in Objects).
    /// { [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: true }
    #[must_use]
    pub const fn builtin_method() -> Self {
        Self(ATTR_WRITABLE | ATTR_CONFIGURABLE)
    }

    /// Function `.length` and `.name` properties (§10.2.8 SetFunctionLength).
    /// { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }
    #[must_use]
    pub const fn function_length() -> Self {
        Self(ATTR_CONFIGURABLE)
    }

    /// `prototype.constructor` link (§10.2.6 MakeConstructor step 8).
    /// { [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: true }
    #[must_use]
    pub const fn constructor_link() -> Self {
        Self(ATTR_WRITABLE | ATTR_CONFIGURABLE)
    }

    /// Constructor function `.prototype` property (§10.2.6 MakeConstructor step 6).
    /// { [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: false }
    #[must_use]
    pub const fn function_prototype() -> Self {
        Self(ATTR_WRITABLE)
    }

    /// Non-writable, non-enumerable, non-configurable (§21.3.1 Math value properties).
    /// { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }
    #[must_use]
    pub const fn constant() -> Self {
        Self(0)
    }

    /// Array `length` property (§10.4.2.4 ArraySetLength).
    /// { [[Writable]]: true, [[Enumerable]]: false, [[Configurable]]: false }
    #[must_use]
    pub const fn array_length() -> Self {
        Self(ATTR_WRITABLE)
    }

    /// Frozen data property (§20.1.2.6 Object.freeze).
    /// { [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: false }
    #[must_use]
    pub const fn frozen() -> Self {
        Self(0)
    }

    /// Built-in accessor properties (§10.4.1).
    /// Accessors have no [[Writable]]; enumerable=false, configurable=true.
    #[must_use]
    pub const fn builtin_accessor() -> Self {
        Self(ATTR_CONFIGURABLE)
    }

    /// Construct from individual flags.
    #[must_use]
    pub const fn from_flags(writable: bool, enumerable: bool, configurable: bool) -> Self {
        let mut bits = 0u8;
        if writable {
            bits |= ATTR_WRITABLE;
        }
        if enumerable {
            bits |= ATTR_ENUMERABLE;
        }
        if configurable {
            bits |= ATTR_CONFIGURABLE;
        }
        Self(bits)
    }

    #[must_use]
    pub const fn writable(self) -> bool {
        self.0 & ATTR_WRITABLE != 0
    }

    #[must_use]
    pub const fn enumerable(self) -> bool {
        self.0 & ATTR_ENUMERABLE != 0
    }

    #[must_use]
    pub const fn configurable(self) -> bool {
        self.0 & ATTR_CONFIGURABLE != 0
    }

    /// Returns a copy with writable set to `false`.
    #[must_use]
    pub const fn with_writable_false(self) -> Self {
        Self(self.0 & !ATTR_WRITABLE)
    }

    /// Returns a copy with configurable set to `false`.
    #[must_use]
    pub const fn with_configurable_false(self) -> Self {
        Self(self.0 & !ATTR_CONFIGURABLE)
    }
}

impl Default for PropertyAttributes {
    fn default() -> Self {
        Self::data()
    }
}

/// Property slot stored on ordinary or host-function objects.
///
/// Each property carries its own [`PropertyAttributes`] per ES2024 §6.1.7.1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropertyValue {
    Data {
        value: RegisterValue,
        attributes: PropertyAttributes,
    },
    Accessor {
        getter: Option<ObjectHandle>,
        setter: Option<ObjectHandle>,
        attributes: PropertyAttributes,
    },
}

impl PropertyValue {
    /// Creates a data property with default attributes (writable + enumerable + configurable).
    #[must_use]
    pub const fn data(value: RegisterValue) -> Self {
        Self::Data {
            value,
            attributes: PropertyAttributes::data(),
        }
    }

    /// Creates a data property with explicit attributes.
    #[must_use]
    pub const fn data_with_attrs(value: RegisterValue, attributes: PropertyAttributes) -> Self {
        Self::Data { value, attributes }
    }

    /// Creates an accessor property with default builtin accessor attributes.
    #[must_use]
    pub const fn accessor(getter: Option<ObjectHandle>, setter: Option<ObjectHandle>) -> Self {
        Self::Accessor {
            getter,
            setter,
            attributes: PropertyAttributes::builtin_accessor(),
        }
    }

    /// Returns the attributes of this property.
    #[must_use]
    pub const fn attributes(&self) -> PropertyAttributes {
        match self {
            Self::Data { attributes, .. } | Self::Accessor { attributes, .. } => *attributes,
        }
    }
}

/// ES2024 §6.2.6 Property Descriptor record with field presence preserved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropertyDescriptorKind {
    Generic,
    Data {
        value: Option<RegisterValue>,
        writable: Option<bool>,
    },
    Accessor {
        getter: Option<Option<ObjectHandle>>,
        setter: Option<Option<ObjectHandle>>,
    },
}

/// Partial property descriptor used by `[[DefineOwnProperty]]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropertyDescriptor {
    kind: PropertyDescriptorKind,
    enumerable: Option<bool>,
    configurable: Option<bool>,
}

impl PropertyDescriptor {
    #[must_use]
    pub const fn generic(enumerable: Option<bool>, configurable: Option<bool>) -> Self {
        Self {
            kind: PropertyDescriptorKind::Generic,
            enumerable,
            configurable,
        }
    }

    #[must_use]
    pub const fn data(
        value: Option<RegisterValue>,
        writable: Option<bool>,
        enumerable: Option<bool>,
        configurable: Option<bool>,
    ) -> Self {
        Self {
            kind: PropertyDescriptorKind::Data { value, writable },
            enumerable,
            configurable,
        }
    }

    #[must_use]
    pub const fn accessor(
        getter: Option<Option<ObjectHandle>>,
        setter: Option<Option<ObjectHandle>>,
        enumerable: Option<bool>,
        configurable: Option<bool>,
    ) -> Self {
        Self {
            kind: PropertyDescriptorKind::Accessor { getter, setter },
            enumerable,
            configurable,
        }
    }

    #[must_use]
    pub const fn kind(self) -> PropertyDescriptorKind {
        self.kind
    }

    #[must_use]
    pub const fn enumerable(self) -> Option<bool> {
        self.enumerable
    }

    #[must_use]
    pub const fn configurable(self) -> Option<bool> {
        self.configurable
    }

    /// Converts this partial descriptor into a concrete `PropertyValue`,
    /// filling in spec-default values for any missing fields.
    /// §6.2.6.1 CompletePropertyDescriptor
    #[must_use]
    pub fn to_property_value(self) -> PropertyValue {
        let enumerable = self.enumerable.unwrap_or(false);
        let configurable = self.configurable.unwrap_or(false);
        match self.kind {
            PropertyDescriptorKind::Generic => PropertyValue::data_with_attrs(
                RegisterValue::undefined(),
                PropertyAttributes::from_flags(false, enumerable, configurable),
            ),
            PropertyDescriptorKind::Data { value, writable } => PropertyValue::data_with_attrs(
                value.unwrap_or_else(RegisterValue::undefined),
                PropertyAttributes::from_flags(writable.unwrap_or(false), enumerable, configurable),
            ),
            PropertyDescriptorKind::Accessor { getter, setter } => PropertyValue::Accessor {
                getter: getter.unwrap_or(None),
                setter: setter.unwrap_or(None),
                attributes: PropertyAttributes::from_flags(false, enumerable, configurable),
            },
        }
    }

    #[must_use]
    pub const fn from_property_value(value: PropertyValue) -> Self {
        let attributes = value.attributes();
        match value {
            PropertyValue::Data { value, .. } => Self::data(
                Some(value),
                Some(attributes.writable()),
                Some(attributes.enumerable()),
                Some(attributes.configurable()),
            ),
            PropertyValue::Accessor { getter, setter, .. } => Self::accessor(
                Some(getter),
                Some(setter),
                Some(attributes.enumerable()),
                Some(attributes.configurable()),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum HeapValue {
    Object {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        /// §9.1 `[[PrivateElements]]` internal slot.
        /// Spec: <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
        private_elements: Vec<(PrivateNameKey, PrivateElement)>,
    },
    NativeObject {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        payload: NativePayloadId,
    },
    Array {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        elements: Vec<RegisterValue>,
        indexed_properties: BTreeMap<usize, PropertyValue>,
        elements_writable: bool,
        elements_configurable: bool,
        length_writable: bool,
    },
    String {
        prototype: Option<ObjectHandle>,
        value: JsString,
    },
    Closure {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        flags: ClosureFlags,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        module: Module,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
        /// §10.2 ECMAScript Function Objects — `[[Realm]]` slot.
        /// The realm in which this closure was created.
        /// Spec: <https://tc39.es/ecma262/#sec-ecmascript-function-objects>
        realm: crate::realm::RealmId,
        /// §15.7.14 — class field initializer closure, if this is a class constructor.
        /// Stored during ClassDefinitionEvaluation, invoked by RunClassFieldInitializer.
        /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
        field_initializer: Option<ObjectHandle>,
        /// §6.2.12 — unique class identity for private name resolution.
        /// Non-zero when this closure is a class constructor or a method
        /// that accesses private names.
        /// Spec: <https://tc39.es/ecma262/#sec-private-names>
        class_id: u64,
        /// §15.7.14 `[[PrivateMethods]]` — private methods/accessors to copy
        /// to instances during InitializeInstanceElements.
        /// Spec: <https://tc39.es/ecma262/#sec-initializeinstanceelements>
        private_methods: Vec<(PrivateNameKey, PrivateElement)>,
        /// §9.1 `[[PrivateElements]]` — private elements on the closure itself
        /// (used for static private fields/methods/accessors on constructors).
        /// Spec: <https://tc39.es/ecma262/#sec-ordinary-object-internal-methods-and-internal-slots>
        private_elements: Vec<(PrivateNameKey, PrivateElement)>,
    },
    HostFunction {
        function: HostFunctionId,
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        /// §10.2 ECMAScript Function Objects — `[[Realm]]` slot.
        /// The realm in which this host function was installed.
        /// Spec: <https://tc39.es/ecma262/#sec-ecmascript-function-objects>
        realm: crate::realm::RealmId,
    },
    /// ES2024 §10.4.1 Bound Function Exotic Objects.
    BoundFunction {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        target: ObjectHandle,
        bound_this: RegisterValue,
        bound_args: Vec<RegisterValue>,
        /// §10.4.1 Bound Function Exotic Objects — `[[Realm]]` slot.
        /// Spec: <https://tc39.es/ecma262/#sec-bound-function-exotic-objects>
        realm: crate::realm::RealmId,
    },
    UpvalueCell {
        value: RegisterValue,
    },
    /// §6.1.6.2 The BigInt Type — heap-allocated arbitrary-precision integer.
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    BigInt {
        value: Box<str>,
    },
    /// V8 extension — captured stack frame snapshots for `Error.prototype.stack`.
    /// Stored as a heap value so it can be referenced from a non-enumerable
    /// data property on the error instance and reach `format_v8_stack`.
    /// Reference: <https://v8.dev/docs/stack-trace-api>
    ErrorStackFrames {
        frames: Vec<crate::stack_frame::StackFrameInfo>,
    },
    /// ES2024 §23.1.5.1 — Array Iterator Objects.
    ArrayIterator {
        prototype: Option<ObjectHandle>,
        iterable: ObjectHandle,
        next_index: usize,
        closed: bool,
        kind: ArrayIteratorKind,
    },
    /// ES2024 §22.1.5.1 — String Iterator Objects.
    StringIterator {
        prototype: Option<ObjectHandle>,
        iterable: ObjectHandle,
        next_index: usize,
        closed: bool,
    },
    PropertyIterator {
        key_handles: Vec<ObjectHandle>,
        next_index: usize,
    },
    Promise {
        prototype: Option<ObjectHandle>,
        promise: crate::promise::JsPromise,
    },
    /// ES2024 §24.1 Map Objects — insertion-ordered key-value pairs with SameValueZero.
    Map {
        prototype: Option<ObjectHandle>,
        entries: Vec<Option<(RegisterValue, RegisterValue)>>,
    },
    /// ES2024 §24.2 Set Objects — insertion-ordered unique values with SameValueZero.
    Set {
        prototype: Option<ObjectHandle>,
        entries: Vec<Option<RegisterValue>>,
    },
    /// ES2024 §24.1.5.1 — Map Iterator Objects.
    MapIterator {
        prototype: Option<ObjectHandle>,
        iterable: ObjectHandle,
        next_index: usize,
        closed: bool,
        kind: MapIteratorKind,
    },
    /// ES2024 §24.2.5.1 — Set Iterator Objects.
    SetIterator {
        prototype: Option<ObjectHandle>,
        iterable: ObjectHandle,
        next_index: usize,
        closed: bool,
        kind: SetIteratorKind,
    },
    /// ES2024 §24.3 WeakMap Objects — weak key-value pairs by handle identity.
    /// Spec: <https://tc39.es/ecma262/#sec-weakmap-objects>
    WeakMap {
        prototype: Option<ObjectHandle>,
        /// Maps key ObjectHandle index to value. Keys must be objects.
        entries: std::collections::HashMap<u32, RegisterValue>,
    },
    /// ES2024 §24.4 WeakSet Objects — weak value set by handle identity.
    /// Spec: <https://tc39.es/ecma262/#sec-weakset-objects>
    WeakSet {
        prototype: Option<ObjectHandle>,
        /// Stores ObjectHandle indices. Keys must be objects.
        entries: std::collections::HashSet<u32>,
    },
    /// ES2024 §26.1 WeakRef Objects — weak reference to a target object.
    /// Spec: <https://tc39.es/ecma262/#sec-weak-ref-objects>
    WeakRef {
        prototype: Option<ObjectHandle>,
        /// The weakly-held target. Set to `None` when the target is collected.
        target: Option<u32>,
    },
    /// ES2024 §26.2 FinalizationRegistry Objects — invoke cleanup after target collection.
    /// Spec: <https://tc39.es/ecma262/#sec-finalization-registry-objects>
    FinalizationRegistry {
        prototype: Option<ObjectHandle>,
        /// The cleanup callback function.
        cleanup_callback: ObjectHandle,
        /// Registered cells: (target_handle, held_value, unregister_token).
        /// Target is `None` when already collected and pending cleanup.
        cells: Vec<FinalizationCell>,
    },
    /// ES2024 §27.5 — Generator Objects.
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-generator-instances>
    Generator {
        prototype: Option<ObjectHandle>,
        /// The generator's execution state.
        state: GeneratorState,
        /// Module containing the generator function's bytecode.
        module: Module,
        /// Function index of the generator function within the module.
        function_index: FunctionIndex,
        /// Closure handle (if the generator function was a closure).
        closure_handle: Option<ObjectHandle>,
        /// Arguments passed when the generator function was called.
        /// Available for the first `.next()` which starts execution.
        arguments: Vec<RegisterValue>,
        /// Saved register window (captured at yield, restored at resume).
        registers: Option<Box<[RegisterValue]>>,
        /// Program counter to resume from.
        resume_pc: ProgramCounter,
        /// Register index where the sent value should be written on resume.
        resume_register: u16,
        /// §14.4.4 `yield*` — active delegation iterator, if any.
        /// When set, `.next()/.return()/.throw()` are forwarded to this inner
        /// iterator instead of resuming the generator body directly.
        /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>
        delegation_iterator: Option<ObjectHandle>,
    },
    /// ES2024 §27.6 — AsyncGenerator Objects.
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-objects>
    ///
    /// Like Generator but `.next()/.return()/.throw()` return Promises.
    /// The request queue holds pending `{completion, capability}` pairs.
    AsyncGenerator {
        prototype: Option<ObjectHandle>,
        /// The async generator's execution state.
        state: GeneratorState,
        /// Module containing the generator function's bytecode.
        module: Module,
        /// Function index within the module.
        function_index: FunctionIndex,
        /// Closure handle (if the async generator function was a closure).
        closure_handle: Option<ObjectHandle>,
        /// Arguments passed when the async generator function was called.
        arguments: Vec<RegisterValue>,
        /// Saved register window (captured at yield/await, restored on resume).
        registers: Option<Box<[RegisterValue]>>,
        /// Program counter to resume from.
        resume_pc: ProgramCounter,
        /// Register index where the sent value should be written on resume.
        resume_register: u16,
        /// §27.6.3.2 AsyncGeneratorRequest queue — pending `.next()/.return()/.throw()`.
        /// Each entry holds `(kind, value, promise_handle)`.
        queue: Vec<AsyncGeneratorRequest>,
        /// §14.4.4 `yield*` — active delegation iterator, if any.
        delegation_iterator: Option<ObjectHandle>,
    },
    /// ES2024 §27.2.1.5 — Internal resolve/reject function for a PromiseCapability.
    /// Spec: <https://tc39.es/ecma262/#sec-promise-resolve-functions>
    /// Spec: <https://tc39.es/ecma262/#sec-promise-reject-functions>
    PromiseCapabilityFunction {
        /// The promise this function settles.
        promise: ObjectHandle,
        /// Whether this is the resolve or reject function.
        kind: crate::promise::ReactionKind,
    },
    /// ES2024 §27.2.4 per-element functions for Promise.all/allSettled/any.
    PromiseCombinatorElement {
        combinator_kind: crate::promise::PromiseCombinatorKind,
        index: u32,
        result_array: ObjectHandle,
        remaining_counter: ObjectHandle,
        result_capability: crate::promise::PromiseCapability,
        already_called: bool,
    },
    /// ES2024 §27.2.5.3.1–2 wrapper for Promise.prototype.finally.
    PromiseFinallyFunction {
        on_finally: ObjectHandle,
        constructor: ObjectHandle,
        kind: crate::promise::PromiseFinallyKind,
    },
    /// Thunk that returns or throws a captured value (for finally chaining).
    PromiseValueThunk {
        value: crate::value::RegisterValue,
        kind: crate::promise::PromiseFinallyKind,
    },
    /// ES2024 §25.1 ArrayBuffer Objects.
    /// Spec: <https://tc39.es/ecma262/#sec-arraybuffer-objects>
    ArrayBuffer {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        data: Vec<u8>,
        /// §25.1.2.1 [[ArrayBufferDetachKey]] — true after detach/transfer.
        detached: bool,
        /// §25.1.3.1 step 9 — configured maximum for resizable buffers.
        max_byte_length: usize,
        /// §25.1.3.1 step 10 — true when constructed with maxByteLength option.
        resizable: bool,
    },
    /// ES2024 §25.2 SharedArrayBuffer Objects.
    /// Spec: <https://tc39.es/ecma262/#sec-sharedarraybuffer-objects>
    SharedArrayBuffer {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        data: Vec<u8>,
        max_byte_length: usize,
        growable: bool,
    },
    /// ES2024 §22.2 RegExp Objects — ordinary object with [[OriginalSource]] and [[OriginalFlags]].
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-regexp-instances>
    RegExp {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        /// The pattern string without surrounding `/` characters.
        pattern: Box<str>,
        /// Canonical flags string (alphabetically sorted).
        flags: Box<str>,
    },
    /// ES2024 §25.3 DataView Objects.
    /// Spec: <https://tc39.es/ecma262/#sec-dataview-objects>
    DataView {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        /// [[ViewedArrayBuffer]] — the underlying ArrayBuffer or SharedArrayBuffer.
        viewed_buffer: ObjectHandle,
        /// [[ByteOffset]] — start offset into the buffer.
        byte_offset: usize,
        /// [[ByteLength]] — None means AUTO (tracks buffer length for resizable buffers).
        byte_length: Option<usize>,
    },
    /// ES2024 §23.2 TypedArray Objects (Int8Array, Uint8Array, etc.).
    /// Spec: <https://tc39.es/ecma262/#sec-typedarray-objects>
    TypedArray {
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        /// [[ViewedArrayBuffer]] — the underlying ArrayBuffer or SharedArrayBuffer.
        viewed_buffer: ObjectHandle,
        /// [[ByteOffset]] — byte offset into the buffer.
        byte_offset: usize,
        /// [[ByteLength]] — byte length of the view.
        byte_length: usize,
        /// [[ArrayLength]] — number of elements.
        array_length: usize,
        /// [[TypedArrayName]] / [[ContentType]] — element type discriminator.
        kind: TypedArrayKind,
    },
    /// ES2024 §28.2 Proxy Exotic Objects.
    /// Spec: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots>
    Proxy {
        /// [[ProxyTarget]] — the wrapped target object.
        target: ObjectHandle,
        /// [[ProxyHandler]] — the handler object whose methods define trap behavior.
        handler: ObjectHandle,
        /// `true` after `Proxy.revocable().revoke()` has been called.
        revoked: bool,
    },
}

/// §25.1.2.8 RawBytesToNumeric — read a single typed element from a byte slice.
///
/// Returns the element value as `f64` for numeric types.
/// BigInt64/BigUint64 are returned as their nearest f64 approximation.
fn read_typed_element(kind: TypedArrayKind, bytes: &[u8]) -> f64 {
    match kind {
        TypedArrayKind::Int8 => f64::from(i8::from_ne_bytes([bytes[0]])),
        TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => f64::from(bytes[0]),
        TypedArrayKind::Int16 => f64::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        TypedArrayKind::Uint16 => f64::from(u16::from_le_bytes([bytes[0], bytes[1]])),
        TypedArrayKind::Int32 => {
            f64::from(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        TypedArrayKind::Uint32 => {
            f64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        TypedArrayKind::Float32 => {
            f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        TypedArrayKind::Float64 => f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]),
        TypedArrayKind::BigInt64 => i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]) as f64,
        TypedArrayKind::BigUint64 => u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]) as f64,
    }
}

/// §25.1.2.11 NumericToRawBytes — write a single typed element to a byte slice.
///
/// Implements the spec-correct conversion from `f64` to each element type.
fn write_typed_element(kind: TypedArrayKind, value: f64, bytes: &mut [u8]) {
    match kind {
        TypedArrayKind::Int8 => {
            let n = to_int8(value);
            bytes[0] = n as u8;
        }
        TypedArrayKind::Uint8 => {
            bytes[0] = to_uint8(value);
        }
        TypedArrayKind::Uint8Clamped => {
            bytes[0] = to_uint8_clamp(value);
        }
        TypedArrayKind::Int16 => {
            let n = to_int16(value);
            bytes[..2].copy_from_slice(&n.to_le_bytes());
        }
        TypedArrayKind::Uint16 => {
            let n = to_uint16(value);
            bytes[..2].copy_from_slice(&n.to_le_bytes());
        }
        TypedArrayKind::Int32 => {
            let n = to_int32(value);
            bytes[..4].copy_from_slice(&n.to_le_bytes());
        }
        TypedArrayKind::Uint32 => {
            let n = to_uint32(value);
            bytes[..4].copy_from_slice(&n.to_le_bytes());
        }
        TypedArrayKind::Float32 => {
            let n = value as f32;
            bytes[..4].copy_from_slice(&n.to_le_bytes());
        }
        TypedArrayKind::Float64 => {
            bytes[..8].copy_from_slice(&value.to_le_bytes());
        }
        TypedArrayKind::BigInt64 => {
            let n = value.trunc() as i64;
            bytes[..8].copy_from_slice(&n.to_le_bytes());
        }
        TypedArrayKind::BigUint64 => {
            let n = value.trunc() as u64;
            bytes[..8].copy_from_slice(&n.to_le_bytes());
        }
    }
}

/// §7.1.6 ToInt32 — modular conversion.
fn to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    (n.trunc() as i64) as i32
}

/// §7.1.7 ToUint32 — modular conversion.
fn to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    (n.trunc() as i64) as u32
}

/// §7.1.9 ToInt16 — modular conversion.
fn to_int16(n: f64) -> i16 {
    to_uint32(n) as i16
}

/// §7.1.10 ToUint16 — modular conversion.
fn to_uint16(n: f64) -> u16 {
    to_uint32(n) as u16
}

/// §7.1.8 ToInt8 — modular conversion.
fn to_int8(n: f64) -> i8 {
    to_uint32(n) as i8
}

/// §7.1.11 ToUint8 — modular conversion.
fn to_uint8(n: f64) -> u8 {
    to_uint32(n) as u8
}

/// §7.1.12 ToUint8Clamp — clamped conversion.
fn to_uint8_clamp(n: f64) -> u8 {
    if n.is_nan() || n <= 0.0 {
        return 0;
    }
    if n >= 255.0 {
        return 255;
    }
    let f = n.floor();
    if f + 0.5 < n {
        return (f + 1.0) as u8;
    }
    if n < f + 0.5 {
        return f as u8;
    }
    // Exact 0.5 case: round to even.
    let i = f as u8;
    if i.is_multiple_of(2) { i } else { i + 1 }
}

/// Visit an ObjectHandle as a GcHandle for tracing.
fn trace_handle(handle: ObjectHandle, visitor: &mut dyn FnMut(GcHandle)) {
    visitor(GcHandle(handle.0));
}

/// Visit a RegisterValue that may contain an object handle.
fn trace_register_value(value: RegisterValue, visitor: &mut dyn FnMut(GcHandle)) {
    if let Some(handle) = value.as_object_handle() {
        visitor(GcHandle(handle));
    }
}

/// Visit all GC pointers in a PropertyValue.
fn trace_property_value(pv: &PropertyValue, visitor: &mut dyn FnMut(GcHandle)) {
    match pv {
        PropertyValue::Data { value, .. } => trace_register_value(*value, visitor),
        PropertyValue::Accessor { getter, setter, .. } => {
            if let Some(g) = getter {
                trace_handle(*g, visitor);
            }
            if let Some(s) = setter {
                trace_handle(*s, visitor);
            }
        }
    }
}

/// Visit all GC pointers in a `[[PrivateElements]]` or `[[PrivateMethods]]` list.
fn trace_private_elements(
    elements: &[(PrivateNameKey, PrivateElement)],
    visitor: &mut dyn FnMut(GcHandle),
) {
    for (_, element) in elements {
        match element {
            PrivateElement::Field(value) => trace_register_value(*value, visitor),
            PrivateElement::Method(handle) => trace_handle(*handle, visitor),
            PrivateElement::Accessor { getter, setter } => {
                if let Some(g) = getter {
                    trace_handle(*g, visitor);
                }
                if let Some(s) = setter {
                    trace_handle(*s, visitor);
                }
            }
        }
    }
}

impl Traceable for HeapValue {
    fn trace_handles(&self, visitor: &mut dyn FnMut(GcHandle)) {
        match self {
            HeapValue::Object {
                prototype,
                values,
                private_elements,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for v in values {
                    trace_property_value(v, visitor);
                }
                trace_private_elements(private_elements, visitor);
            }
            HeapValue::NativeObject {
                prototype, values, ..
            }
            | HeapValue::HostFunction {
                prototype, values, ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for v in values {
                    trace_property_value(v, visitor);
                }
            }
            HeapValue::Closure {
                prototype,
                values,
                upvalues,
                field_initializer,
                private_methods,
                private_elements,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for v in values {
                    trace_property_value(v, visitor);
                }
                for uv in upvalues {
                    trace_handle(*uv, visitor);
                }
                if let Some(fi) = field_initializer {
                    trace_handle(*fi, visitor);
                }
                trace_private_elements(private_methods, visitor);
                trace_private_elements(private_elements, visitor);
            }
            HeapValue::Array {
                prototype,
                values,
                elements,
                indexed_properties,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for value in values {
                    trace_property_value(value, visitor);
                }
                for value in indexed_properties.values() {
                    trace_property_value(value, visitor);
                }
                for elem in elements {
                    trace_register_value(*elem, visitor);
                }
            }
            HeapValue::String { prototype, .. } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
            }
            HeapValue::BoundFunction {
                prototype,
                values,
                target,
                bound_this,
                bound_args,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for value in values {
                    trace_property_value(value, visitor);
                }
                trace_handle(*target, visitor);
                trace_register_value(*bound_this, visitor);
                for arg in bound_args {
                    trace_register_value(*arg, visitor);
                }
            }
            HeapValue::UpvalueCell { value } => {
                trace_register_value(*value, visitor);
            }
            HeapValue::ArrayIterator {
                prototype,
                iterable,
                ..
            }
            | HeapValue::StringIterator {
                prototype,
                iterable,
                ..
            }
            | HeapValue::MapIterator {
                prototype,
                iterable,
                ..
            }
            | HeapValue::SetIterator {
                prototype,
                iterable,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                trace_handle(*iterable, visitor);
            }
            HeapValue::PropertyIterator { key_handles, .. } => {
                for h in key_handles {
                    trace_handle(*h, visitor);
                }
            }
            HeapValue::Promise {
                prototype, promise, ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                promise.trace_handles(visitor);
            }
            HeapValue::Map {
                prototype, entries, ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for entry in entries.iter().flatten() {
                    trace_register_value(entry.0, visitor);
                    trace_register_value(entry.1, visitor);
                }
            }
            HeapValue::Set {
                prototype, entries, ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for entry in entries.iter().flatten() {
                    trace_register_value(*entry, visitor);
                }
            }
            HeapValue::WeakMap { prototype, .. }
            | HeapValue::WeakSet { prototype, .. }
            | HeapValue::WeakRef { prototype, .. } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                // Entries/target are intentionally NOT traced — they are weak references.
                // Ephemeron fixpoint in collect_garbage handles value liveness.
            }
            HeapValue::FinalizationRegistry {
                prototype,
                cleanup_callback,
                cells,
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                trace_handle(*cleanup_callback, visitor);
                // Trace held_values (strongly held), but NOT targets/tokens (weak).
                for cell in cells {
                    trace_register_value(cell.held_value, visitor);
                }
            }
            HeapValue::Generator {
                prototype,
                closure_handle,
                registers,
                delegation_iterator,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                if let Some(c) = closure_handle {
                    trace_handle(*c, visitor);
                }
                // Module is not traced (it's an Arc-wrapped shared reference).
                if let Some(regs) = registers {
                    for reg in regs.iter() {
                        trace_register_value(*reg, visitor);
                    }
                }
                if let Some(d) = delegation_iterator {
                    trace_handle(*d, visitor);
                }
            }
            HeapValue::AsyncGenerator {
                prototype,
                closure_handle,
                registers,
                queue,
                delegation_iterator,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                if let Some(c) = closure_handle {
                    trace_handle(*c, visitor);
                }
                if let Some(regs) = registers {
                    for reg in regs.iter() {
                        trace_register_value(*reg, visitor);
                    }
                }
                for req in queue {
                    trace_register_value(req.value, visitor);
                    trace_handle(req.promise, visitor);
                }
                if let Some(d) = delegation_iterator {
                    trace_handle(*d, visitor);
                }
            }
            HeapValue::PromiseCapabilityFunction { promise, .. } => {
                trace_handle(*promise, visitor);
            }
            HeapValue::PromiseCombinatorElement {
                result_array,
                remaining_counter,
                result_capability,
                ..
            } => {
                trace_handle(*result_array, visitor);
                trace_handle(*remaining_counter, visitor);
                visitor(GcHandle(result_capability.promise.0));
                visitor(GcHandle(result_capability.resolve.0));
                visitor(GcHandle(result_capability.reject.0));
            }
            HeapValue::PromiseFinallyFunction {
                on_finally,
                constructor,
                ..
            } => {
                trace_handle(*on_finally, visitor);
                trace_handle(*constructor, visitor);
            }
            HeapValue::PromiseValueThunk { value, .. } => {
                trace_register_value(*value, visitor);
            }
            HeapValue::ArrayBuffer {
                prototype, values, ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for v in values {
                    trace_property_value(v, visitor);
                }
            }
            HeapValue::SharedArrayBuffer {
                prototype, values, ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for v in values {
                    trace_property_value(v, visitor);
                }
            }
            HeapValue::RegExp {
                prototype, values, ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                for v in values {
                    trace_property_value(v, visitor);
                }
            }
            HeapValue::DataView {
                prototype,
                values,
                viewed_buffer,
                ..
            }
            | HeapValue::TypedArray {
                prototype,
                values,
                viewed_buffer,
                ..
            } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                trace_handle(*viewed_buffer, visitor);
                for v in values {
                    trace_property_value(v, visitor);
                }
            }
            HeapValue::Proxy {
                target, handler, ..
            } => {
                trace_handle(*target, visitor);
                trace_handle(*handler, visitor);
            }
            HeapValue::BigInt { .. } => {}
            HeapValue::ErrorStackFrames { .. } => {
                // Stack frame snapshots clone Module by value (Arc), and
                // captured names are owned strings — nothing to trace.
            }
        }
    }
}

/// Object heap backed by the otter-gc TypedHeap for automatic collection.
pub struct ObjectHeap {
    heap: TypedHeap,
    next_shape_id: u64,
}

impl std::fmt::Debug for ObjectHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectHeap")
            .field("live_count", &self.heap.live_count())
            .field("next_shape_id", &self.next_shape_id)
            .finish()
    }
}

impl Default for ObjectHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for ObjectHeap {
    fn eq(&self, _other: &Self) -> bool {
        false // Heaps are never structurally equal
    }
}

impl Clone for ObjectHeap {
    fn clone(&self) -> Self {
        // ObjectHeap is not meaningfully clonable — return a fresh heap.
        // This exists only to satisfy derived trait bounds on parent structs.
        Self::new()
    }
}

impl ObjectHeap {
    /// Creates an empty object heap with the default GC configuration
    /// (no heap cap).
    #[must_use]
    pub fn new() -> Self {
        Self {
            heap: TypedHeap::new(),
            next_shape_id: 1,
        }
    }

    /// Creates an empty object heap backed by a [`TypedHeap`] with the
    /// provided GC configuration. Use this to set a hard heap cap
    /// (`GcConfig::max_heap_bytes`), the Otter analogue of Node's
    /// `--max-old-space-size`.
    #[must_use]
    pub fn with_config(config: GcConfig) -> Self {
        Self {
            heap: TypedHeap::with_config(config),
            next_shape_id: 1,
        }
    }

    /// Returns a clone of the underlying TypedHeap's OOM signal flag.
    /// The interpreter polls this at GC safepoints to raise a catchable
    /// `RangeError` when the hard heap cap has been exceeded.
    pub fn oom_flag(&self) -> Arc<AtomicBool> {
        self.heap.oom_flag()
    }

    /// Clears the OOM signal flag.
    pub fn clear_oom_flag(&self) {
        self.heap.clear_oom_flag();
    }

    /// Returns the configured hard heap cap in bytes, if any.
    pub fn max_heap_bytes(&self) -> Option<usize> {
        self.heap.max_heap_bytes()
    }

    /// Returns the current tracked memory footprint in bytes.
    pub fn tracked_bytes(&self) -> usize {
        self.heap.tracked_bytes()
    }

    /// Reserves `bytes` in the heap budget. Returns `Err(OutOfMemory)` and
    /// sets the OOM flag if the reservation would exceed the hard cap.
    /// Must be paired with [`release_bytes`] when the memory is freed.
    ///
    /// Used by Array growth paths (`Vec::resize` of `elements`) so that
    /// container internals are accounted against the heap cap in addition
    /// to the object shells.
    pub fn reserve_bytes(&mut self, bytes: usize) -> Result<(), OutOfMemory> {
        self.heap.reserve_bytes(bytes)
    }

    /// Releases a previous byte reservation.
    pub fn release_bytes(&mut self, bytes: usize) {
        self.heap.release_bytes(bytes);
    }

    /// Walks every live `HeapValue` and returns a per-variant count. The
    /// total return is `(total_count, total_tracked_bytes, per_variant)`.
    ///
    /// Used by the test262 runner's `--memory-profile` mode to detect
    /// leaks by comparing deltas between snapshots. The walk is O(N) over
    /// the slot table, so callers should take snapshots sparingly (every
    /// N tests, not every test).
    pub fn collect_type_stats(&self) -> HeapTypeStats {
        let mut stats = HeapTypeStats::default();
        let elem_size = std::mem::size_of::<RegisterValue>();
        self.heap.for_each(|_, any| {
            let Some(value) = any.downcast_ref::<HeapValue>() else {
                return;
            };
            stats.total_count += 1;
            let shell = std::mem::size_of::<HeapValue>();
            let (name, extra) = match value {
                HeapValue::Object { values, .. } => ("Object", values.len() * elem_size),
                HeapValue::NativeObject { .. } => ("NativeObject", 0),
                HeapValue::HostFunction { .. } => ("HostFunction", 0),
                HeapValue::Array { elements, .. } => ("Array", elements.len() * elem_size),
                HeapValue::String { .. } => ("String", 0),
                HeapValue::Closure { .. } => ("Closure", 0),
                HeapValue::BoundFunction { .. } => ("BoundFunction", 0),
                HeapValue::UpvalueCell { .. } => ("UpvalueCell", 0),
                HeapValue::ArrayIterator { .. } => ("ArrayIterator", 0),
                HeapValue::StringIterator { .. } => ("StringIterator", 0),
                HeapValue::PropertyIterator { .. } => ("PropertyIterator", 0),
                HeapValue::Promise { .. } => ("Promise", 0),
                HeapValue::PromiseCapabilityFunction { .. } => ("PromiseCapabilityFn", 0),
                HeapValue::PromiseCombinatorElement { .. } => ("PromiseCombinatorElem", 0),
                HeapValue::PromiseFinallyFunction { .. } => ("PromiseFinallyFn", 0),
                HeapValue::PromiseValueThunk { .. } => ("PromiseValueThunk", 0),
                HeapValue::Map { .. } => ("Map", 0),
                HeapValue::Set { .. } => ("Set", 0),
                HeapValue::MapIterator { .. } => ("MapIterator", 0),
                HeapValue::SetIterator { .. } => ("SetIterator", 0),
                HeapValue::WeakMap { .. } => ("WeakMap", 0),
                HeapValue::WeakSet { .. } => ("WeakSet", 0),
                HeapValue::WeakRef { .. } => ("WeakRef", 0),
                HeapValue::FinalizationRegistry { .. } => ("FinalizationRegistry", 0),
                HeapValue::Generator { .. } => ("Generator", 0),
                HeapValue::AsyncGenerator { .. } => ("AsyncGenerator", 0),
                HeapValue::ArrayBuffer { data, .. } => ("ArrayBuffer", data.len()),
                HeapValue::SharedArrayBuffer { data, .. } => ("SharedArrayBuffer", data.len()),
                HeapValue::DataView { .. } => ("DataView", 0),
                HeapValue::TypedArray { .. } => ("TypedArray", 0),
                HeapValue::RegExp { .. } => ("RegExp", 0),
                HeapValue::Proxy { .. } => ("Proxy", 0),
                HeapValue::BigInt { .. } => ("BigInt", 0),
                HeapValue::ErrorStackFrames { .. } => ("ErrorStackFrames", 0),
            };
            let entry = stats.by_type.entry(name).or_default();
            entry.0 += 1;
            entry.1 += shell + extra;
            stats.total_bytes += shell + extra;
        });
        stats
    }

    /// Allocates a plain empty object.
    pub fn alloc_object(&mut self) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::Object {
            prototype: None,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            private_elements: Vec::new(),
        });
        ObjectHandle(h.0)
    }

    /// Allocates an ordinary object that carries one native payload link.
    pub fn alloc_native_object(&mut self, payload: NativePayloadId) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::NativeObject {
            prototype: None,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            payload,
        });
        ObjectHandle(h.0)
    }

    /// Allocates an empty dense array.
    pub fn alloc_array(&mut self) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::Array {
            prototype: None,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            elements: Vec::new(),
            indexed_properties: BTreeMap::new(),
            elements_writable: true,
            elements_configurable: true,
            length_writable: true,
        });
        ObjectHandle(h.0)
    }

    /// Allocates an empty Map object.
    pub fn alloc_map(&mut self, prototype: Option<ObjectHandle>) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::Map {
            prototype,
            entries: Vec::new(),
        });
        ObjectHandle(h.0)
    }

    /// Allocates an empty Set object.
    pub fn alloc_set(&mut self, prototype: Option<ObjectHandle>) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::Set {
            prototype,
            entries: Vec::new(),
        });
        ObjectHandle(h.0)
    }

    /// Allocates a fixed-length ArrayBuffer object with zero-initialized storage.
    ///
    /// §25.1.3.1 ArrayBuffer ( length )
    /// Spec: <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
    pub fn alloc_array_buffer(
        &mut self,
        byte_length: usize,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        self.alloc_array_buffer_full(vec![0; byte_length], byte_length, false, prototype)
    }

    /// Allocates an ArrayBuffer object with explicit backing bytes (fixed-length).
    pub fn alloc_array_buffer_with_data(
        &mut self,
        data: Vec<u8>,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let len = data.len();
        self.alloc_array_buffer_full(data, len, false, prototype)
    }

    /// Allocates a resizable ArrayBuffer with maxByteLength.
    ///
    /// §25.1.3.1 ArrayBuffer ( length [, options] )
    /// Spec: <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
    pub fn alloc_array_buffer_resizable(
        &mut self,
        byte_length: usize,
        max_byte_length: usize,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        self.alloc_array_buffer_full(vec![0; byte_length], max_byte_length, true, prototype)
    }

    /// Allocates an ArrayBuffer with all fields specified.
    pub fn alloc_array_buffer_full(
        &mut self,
        data: Vec<u8>,
        max_byte_length: usize,
        resizable: bool,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::ArrayBuffer {
            prototype,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            data,
            detached: false,
            max_byte_length,
            resizable,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a SharedArrayBuffer with explicit growth metadata.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
    pub fn alloc_shared_array_buffer_with_data(
        &mut self,
        data: Vec<u8>,
        max_byte_length: usize,
        growable: bool,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::SharedArrayBuffer {
            prototype,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            max_byte_length,
            growable,
            data,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a SharedArrayBuffer with zero-initialized storage.
    pub fn alloc_shared_array_buffer(
        &mut self,
        byte_length: usize,
        max_byte_length: usize,
        growable: bool,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        self.alloc_shared_array_buffer_with_data(
            vec![0; byte_length],
            max_byte_length,
            growable,
            prototype,
        )
    }

    /// §25.1.5.1 get ArrayBuffer.prototype.byteLength
    /// Returns 0 for detached buffers per spec.
    /// <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.bytelength>
    pub fn array_buffer_byte_length(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayBuffer { data, detached, .. } => {
                if *detached {
                    Ok(0)
                } else {
                    Ok(data.len())
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §25.1.5.3 get ArrayBuffer.prototype.detached
    /// <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.detached>
    pub fn array_buffer_is_detached(&self, handle: ObjectHandle) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayBuffer { detached, .. } => Ok(*detached),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §25.1.5.4 get ArrayBuffer.prototype.maxByteLength
    /// <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.maxbytelength>
    pub fn array_buffer_max_byte_length(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayBuffer {
                data,
                detached,
                max_byte_length,
                resizable,
                ..
            } => {
                if *detached {
                    Ok(0)
                } else if *resizable {
                    Ok(*max_byte_length)
                } else {
                    Ok(data.len())
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §25.1.5.5 get ArrayBuffer.prototype.resizable
    /// <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.resizable>
    pub fn array_buffer_is_resizable(&self, handle: ObjectHandle) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayBuffer { resizable, .. } => Ok(*resizable),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §25.1.5.6 ArrayBuffer.prototype.resize ( newLength )
    /// <https://tc39.es/ecma262/#sec-arraybuffer.prototype.resize>
    pub fn array_buffer_resize(
        &mut self,
        handle: ObjectHandle,
        new_byte_length: usize,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::ArrayBuffer {
                data,
                detached,
                max_byte_length,
                resizable,
                ..
            } => {
                if *detached {
                    return Err(ObjectError::InvalidKind);
                }
                if !*resizable {
                    return Err(ObjectError::InvalidKind);
                }
                if new_byte_length > *max_byte_length {
                    return Err(ObjectError::InvalidArrayLength);
                }
                data.resize(new_byte_length, 0);
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §25.1.5.7 ArrayBuffer.prototype.transfer ( [ newLength ] )
    /// <https://tc39.es/ecma262/#sec-arraybuffer.prototype.transfer>
    ///
    /// Detaches this buffer and returns (data, old_max_byte_length, old_resizable).
    pub fn array_buffer_detach_for_transfer(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<(Vec<u8>, usize, bool), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::ArrayBuffer {
                data,
                detached,
                max_byte_length,
                resizable,
                ..
            } => {
                if *detached {
                    return Err(ObjectError::InvalidKind);
                }
                let old_data = std::mem::take(data);
                let old_max = *max_byte_length;
                let old_resizable = *resizable;
                *detached = true;
                *max_byte_length = 0;
                Ok((old_data, old_max, old_resizable))
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Raw byte access (immutable) to array buffer data.
    /// Returns None if detached.
    pub fn array_buffer_data(&self, handle: ObjectHandle) -> Result<Option<&[u8]>, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayBuffer { data, detached, .. } => {
                if *detached {
                    Ok(None)
                } else {
                    Ok(Some(data.as_slice()))
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Clones a byte range from one ArrayBuffer into a fresh ArrayBuffer.
    ///
    /// §25.1.5.4 ArrayBuffer.prototype.slice ( start, end )
    /// <https://tc39.es/ecma262/#sec-arraybuffer.prototype.slice>
    pub fn array_buffer_slice(
        &mut self,
        handle: ObjectHandle,
        start: usize,
        end: usize,
        prototype: Option<ObjectHandle>,
    ) -> Result<ObjectHandle, ObjectError> {
        let data = match self.object(handle)? {
            HeapValue::ArrayBuffer { data, detached, .. } => {
                if *detached {
                    return Err(ObjectError::InvalidKind);
                }
                data
            }
            _ => return Err(ObjectError::InvalidKind),
        };
        let start = start.min(data.len());
        let end = end.min(data.len());
        let bytes = if end < start {
            Vec::new()
        } else {
            data[start..end].to_vec()
        };
        Ok(self.alloc_array_buffer_with_data(bytes, prototype))
    }

    /// Returns the current byte length of a SharedArrayBuffer.
    pub fn shared_array_buffer_byte_length(
        &self,
        handle: ObjectHandle,
    ) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::SharedArrayBuffer { data, .. } => Ok(data.len()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the configured maximum byte length of a SharedArrayBuffer.
    pub fn shared_array_buffer_max_byte_length(
        &self,
        handle: ObjectHandle,
    ) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::SharedArrayBuffer {
                data,
                max_byte_length,
                growable,
                ..
            } => {
                if *growable {
                    Ok(*max_byte_length)
                } else {
                    Ok(data.len())
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns whether a SharedArrayBuffer is growable.
    pub fn shared_array_buffer_is_growable(
        &self,
        handle: ObjectHandle,
    ) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::SharedArrayBuffer { growable, .. } => Ok(*growable),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Grows a SharedArrayBuffer in place up to its maximum length.
    pub fn shared_array_buffer_grow(
        &mut self,
        handle: ObjectHandle,
        new_byte_length: usize,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::SharedArrayBuffer {
                data,
                max_byte_length,
                growable,
                ..
            } => {
                if !*growable {
                    return Err(ObjectError::InvalidKind);
                }
                if new_byte_length < data.len() || new_byte_length > *max_byte_length {
                    return Err(ObjectError::InvalidArrayLength);
                }
                data.resize(new_byte_length, 0);
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Clones a byte range from one SharedArrayBuffer into a fresh SharedArrayBuffer.
    pub fn shared_array_buffer_slice(
        &mut self,
        handle: ObjectHandle,
        start: usize,
        end: usize,
        max_byte_length: usize,
        growable: bool,
        prototype: Option<ObjectHandle>,
    ) -> Result<ObjectHandle, ObjectError> {
        let data = match self.object(handle)? {
            HeapValue::SharedArrayBuffer { data, .. } => data,
            _ => return Err(ObjectError::InvalidKind),
        };
        let start = start.min(data.len());
        let end = end.min(data.len());
        let bytes = if end < start {
            Vec::new()
        } else {
            data[start..end].to_vec()
        };
        Ok(self.alloc_shared_array_buffer_with_data(bytes, max_byte_length, growable, prototype))
    }

    // ── DataView ──────────────────────────────────────────────────────

    /// §25.3.2.1 DataView ( buffer, byteOffset, byteLength )
    /// <https://tc39.es/ecma262/#sec-dataview-constructor>
    pub fn alloc_data_view(
        &mut self,
        viewed_buffer: ObjectHandle,
        byte_offset: usize,
        byte_length: Option<usize>,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::DataView {
            prototype,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            viewed_buffer,
            byte_offset,
            byte_length,
        });
        ObjectHandle(h.0)
    }

    /// §25.3.4.1 get DataView.prototype.buffer
    /// <https://tc39.es/ecma262/#sec-get-dataview.prototype.buffer>
    pub fn data_view_buffer(&self, handle: ObjectHandle) -> Result<ObjectHandle, ObjectError> {
        match self.object(handle)? {
            HeapValue::DataView { viewed_buffer, .. } => Ok(*viewed_buffer),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §25.3.4.2 get DataView.prototype.byteLength
    /// <https://tc39.es/ecma262/#sec-get-dataview.prototype.bytelength>
    ///
    /// Returns the effective byte length, resolving AUTO for resizable buffers.
    pub fn data_view_byte_length(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::DataView {
                viewed_buffer,
                byte_offset,
                byte_length,
                ..
            } => {
                match *byte_length {
                    Some(len) => Ok(len),
                    None => {
                        // AUTO: track the buffer's current byte length.
                        let buf_len = self.array_buffer_or_shared_byte_length(*viewed_buffer)?;
                        Ok(buf_len.saturating_sub(*byte_offset))
                    }
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §25.3.4.3 get DataView.prototype.byteOffset
    /// <https://tc39.es/ecma262/#sec-get-dataview.prototype.byteoffset>
    pub fn data_view_byte_offset(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::DataView { byte_offset, .. } => Ok(*byte_offset),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the viewed buffer handle for a DataView.
    pub fn data_view_viewed_buffer(
        &self,
        handle: ObjectHandle,
    ) -> Result<ObjectHandle, ObjectError> {
        match self.object(handle)? {
            HeapValue::DataView { viewed_buffer, .. } => Ok(*viewed_buffer),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Read bytes from the underlying buffer of a DataView.
    /// Returns a slice of `element_size` bytes at `byte_index` within the buffer.
    pub fn data_view_get_bytes(
        &self,
        handle: ObjectHandle,
        byte_index: usize,
        element_size: usize,
    ) -> Result<Vec<u8>, ObjectError> {
        let (viewed_buffer, byte_offset, view_byte_length) = match self.object(handle)? {
            HeapValue::DataView {
                viewed_buffer,
                byte_offset,
                byte_length,
                ..
            } => {
                let effective_len = match *byte_length {
                    Some(len) => len,
                    None => {
                        let buf_len = self.array_buffer_or_shared_byte_length(*viewed_buffer)?;
                        buf_len.saturating_sub(*byte_offset)
                    }
                };
                (*viewed_buffer, *byte_offset, effective_len)
            }
            _ => return Err(ObjectError::InvalidKind),
        };
        if byte_index + element_size > view_byte_length {
            return Err(ObjectError::InvalidArrayLength);
        }
        let buffer_index = byte_offset + byte_index;
        let data = self.array_buffer_or_shared_data(viewed_buffer)?;
        if buffer_index + element_size > data.len() {
            return Err(ObjectError::InvalidArrayLength);
        }
        Ok(data[buffer_index..buffer_index + element_size].to_vec())
    }

    /// Write bytes into the underlying buffer of a DataView.
    pub fn data_view_set_bytes(
        &mut self,
        handle: ObjectHandle,
        byte_index: usize,
        bytes: &[u8],
    ) -> Result<(), ObjectError> {
        let (viewed_buffer, byte_offset, view_byte_length) = match self.object(handle)? {
            HeapValue::DataView {
                viewed_buffer,
                byte_offset,
                byte_length,
                ..
            } => {
                let effective_len = match *byte_length {
                    Some(len) => len,
                    None => {
                        let buf_len = self.array_buffer_or_shared_byte_length(*viewed_buffer)?;
                        buf_len.saturating_sub(*byte_offset)
                    }
                };
                (*viewed_buffer, *byte_offset, effective_len)
            }
            _ => return Err(ObjectError::InvalidKind),
        };
        let element_size = bytes.len();
        if byte_index + element_size > view_byte_length {
            return Err(ObjectError::InvalidArrayLength);
        }
        let buffer_index = byte_offset + byte_index;
        let data = self.array_buffer_or_shared_data_mut(viewed_buffer)?;
        if buffer_index + element_size > data.len() {
            return Err(ObjectError::InvalidArrayLength);
        }
        data[buffer_index..buffer_index + element_size].copy_from_slice(bytes);
        Ok(())
    }

    /// Helper: get byte length of either ArrayBuffer or SharedArrayBuffer.
    fn array_buffer_or_shared_byte_length(
        &self,
        handle: ObjectHandle,
    ) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayBuffer { data, detached, .. } => {
                if *detached {
                    Ok(0)
                } else {
                    Ok(data.len())
                }
            }
            HeapValue::SharedArrayBuffer { data, .. } => Ok(data.len()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Helper: get immutable data slice from ArrayBuffer or SharedArrayBuffer.
    pub fn array_buffer_or_shared_data(&self, handle: ObjectHandle) -> Result<&[u8], ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayBuffer { data, detached, .. } => {
                if *detached {
                    Err(ObjectError::InvalidKind)
                } else {
                    Ok(data.as_slice())
                }
            }
            HeapValue::SharedArrayBuffer { data, .. } => Ok(data.as_slice()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Helper: get mutable data slice from ArrayBuffer or SharedArrayBuffer.
    pub fn array_buffer_or_shared_data_mut(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<&mut [u8], ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::ArrayBuffer { data, detached, .. } => {
                if *detached {
                    Err(ObjectError::InvalidKind)
                } else {
                    Ok(data.as_mut_slice())
                }
            }
            HeapValue::SharedArrayBuffer { data, .. } => Ok(data.as_mut_slice()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    // ── TypedArray ────────────────────────────────────────────────────

    /// Allocates a TypedArray object backed by the given buffer.
    ///
    /// §23.2.5 Properties of TypedArray Instances.
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-typedarray-instances>
    pub fn alloc_typed_array(
        &mut self,
        kind: TypedArrayKind,
        viewed_buffer: ObjectHandle,
        byte_offset: usize,
        array_length: usize,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let byte_length = array_length * kind.element_size();
        let h = self.heap.alloc(HeapValue::TypedArray {
            prototype,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            viewed_buffer,
            byte_offset,
            byte_length,
            array_length,
            kind,
        });
        ObjectHandle(h.0)
    }

    /// Returns the TypedArrayKind for the given typed array handle.
    pub fn typed_array_kind(&self, handle: ObjectHandle) -> Result<TypedArrayKind, ObjectError> {
        match self.object(handle)? {
            HeapValue::TypedArray { kind, .. } => Ok(*kind),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §23.2.3.1 get %TypedArray%.prototype.buffer
    /// <https://tc39.es/ecma262/#sec-get-%typedarray%.prototype.buffer>
    pub fn typed_array_buffer(&self, handle: ObjectHandle) -> Result<ObjectHandle, ObjectError> {
        match self.object(handle)? {
            HeapValue::TypedArray { viewed_buffer, .. } => Ok(*viewed_buffer),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §23.2.3.2 get %TypedArray%.prototype.byteLength
    /// <https://tc39.es/ecma262/#sec-get-%typedarray%.prototype.bytelength>
    pub fn typed_array_byte_length(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::TypedArray {
                viewed_buffer,
                byte_length,
                ..
            } => {
                // If the buffer is detached, return 0.
                if let Ok(true) = self.array_buffer_is_detached(*viewed_buffer) {
                    Ok(0)
                } else {
                    Ok(*byte_length)
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §23.2.3.3 get %TypedArray%.prototype.byteOffset
    /// <https://tc39.es/ecma262/#sec-get-%typedarray%.prototype.byteoffset>
    pub fn typed_array_byte_offset(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::TypedArray {
                viewed_buffer,
                byte_offset,
                ..
            } => {
                if let Ok(true) = self.array_buffer_is_detached(*viewed_buffer) {
                    Ok(0)
                } else {
                    Ok(*byte_offset)
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §23.2.3.18 get %TypedArray%.prototype.length
    /// <https://tc39.es/ecma262/#sec-get-%typedarray%.prototype.length>
    pub fn typed_array_length(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::TypedArray {
                viewed_buffer,
                array_length,
                ..
            } => {
                if let Ok(true) = self.array_buffer_is_detached(*viewed_buffer) {
                    Ok(0)
                } else {
                    Ok(*array_length)
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the viewed_buffer handle for a TypedArray.
    pub fn typed_array_viewed_buffer(
        &self,
        handle: ObjectHandle,
    ) -> Result<ObjectHandle, ObjectError> {
        match self.object(handle)? {
            HeapValue::TypedArray { viewed_buffer, .. } => Ok(*viewed_buffer),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Reads a single element from a TypedArray as an f64.
    ///
    /// §10.4.5.9 IntegerIndexedElementGet
    /// <https://tc39.es/ecma262/#sec-integerindexedelementget>
    pub fn typed_array_get_element(
        &self,
        handle: ObjectHandle,
        index: usize,
    ) -> Result<Option<f64>, ObjectError> {
        let (viewed_buffer, byte_offset, array_length, kind) = match self.object(handle)? {
            HeapValue::TypedArray {
                viewed_buffer,
                byte_offset,
                array_length,
                kind,
                ..
            } => (*viewed_buffer, *byte_offset, *array_length, *kind),
            _ => return Err(ObjectError::InvalidKind),
        };
        if index >= array_length {
            return Ok(None);
        }
        let buffer_data = self.array_buffer_or_shared_data(viewed_buffer)?;
        let elem_size = kind.element_size();
        let byte_index = byte_offset + index * elem_size;
        if byte_index + elem_size > buffer_data.len() {
            return Ok(None);
        }
        let bytes = &buffer_data[byte_index..byte_index + elem_size];
        Ok(Some(read_typed_element(kind, bytes)))
    }

    /// Writes a single element to a TypedArray from an f64.
    ///
    /// §10.4.5.10 IntegerIndexedElementSet
    /// <https://tc39.es/ecma262/#sec-integerindexedelementset>
    pub fn typed_array_set_element(
        &mut self,
        handle: ObjectHandle,
        index: usize,
        value: f64,
    ) -> Result<(), ObjectError> {
        let (viewed_buffer, byte_offset, array_length, kind) = match self.object(handle)? {
            HeapValue::TypedArray {
                viewed_buffer,
                byte_offset,
                array_length,
                kind,
                ..
            } => (*viewed_buffer, *byte_offset, *array_length, *kind),
            _ => return Err(ObjectError::InvalidKind),
        };
        if index >= array_length {
            return Err(ObjectError::InvalidIndex);
        }
        let elem_size = kind.element_size();
        let byte_index = byte_offset + index * elem_size;
        let buffer_data = self.array_buffer_or_shared_data_mut(viewed_buffer)?;
        if byte_index + elem_size > buffer_data.len() {
            return Err(ObjectError::InvalidIndex);
        }
        write_typed_element(
            kind,
            value,
            &mut buffer_data[byte_index..byte_index + elem_size],
        );
        Ok(())
    }

    /// Reads a single element from a TypedArray as a `RegisterValue`.
    ///
    /// For numeric kinds (Int8..Float64), returns a Number.
    /// For BigInt kinds (BigInt64Array, BigUint64Array), allocates and returns a BigInt.
    ///
    /// §10.4.5.9 IntegerIndexedElementGet
    /// <https://tc39.es/ecma262/#sec-integerindexedelementget>
    pub fn typed_array_get_element_value(
        &mut self,
        handle: ObjectHandle,
        index: usize,
    ) -> Result<Option<RegisterValue>, ObjectError> {
        let (viewed_buffer, byte_offset, array_length, kind) = match self.object(handle)? {
            HeapValue::TypedArray {
                viewed_buffer,
                byte_offset,
                array_length,
                kind,
                ..
            } => (*viewed_buffer, *byte_offset, *array_length, *kind),
            _ => return Err(ObjectError::InvalidKind),
        };
        if index >= array_length {
            return Ok(None);
        }
        let buffer_data = self.array_buffer_or_shared_data(viewed_buffer)?;
        let elem_size = kind.element_size();
        let byte_index = byte_offset + index * elem_size;
        if byte_index + elem_size > buffer_data.len() {
            return Ok(None);
        }
        let bytes = &buffer_data[byte_index..byte_index + elem_size];
        if kind.is_bigint_kind() {
            let value_str = match kind {
                TypedArrayKind::BigInt64 => {
                    let raw = i64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    raw.to_string()
                }
                TypedArrayKind::BigUint64 => {
                    let raw = u64::from_le_bytes([
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                        bytes[7],
                    ]);
                    raw.to_string()
                }
                _ => unreachable!(),
            };
            let bigint_handle = self.alloc_bigint(value_str);
            Ok(Some(RegisterValue::from_bigint_handle(bigint_handle.0)))
        } else {
            Ok(Some(RegisterValue::from_number(read_typed_element(
                kind, bytes,
            ))))
        }
    }

    /// Writes a single element to a TypedArray from a `RegisterValue`.
    ///
    /// For numeric kinds, the value is converted to Number (f64).
    /// For BigInt kinds, the value must be a BigInt; its decimal string is parsed.
    ///
    /// §10.4.5.10 IntegerIndexedElementSet
    /// <https://tc39.es/ecma262/#sec-integerindexedelementset>
    pub fn typed_array_set_element_bigint(
        &mut self,
        handle: ObjectHandle,
        index: usize,
        bigint_handle: ObjectHandle,
    ) -> Result<(), ObjectError> {
        let (viewed_buffer, byte_offset, array_length, kind) = match self.object(handle)? {
            HeapValue::TypedArray {
                viewed_buffer,
                byte_offset,
                array_length,
                kind,
                ..
            } => (*viewed_buffer, *byte_offset, *array_length, *kind),
            _ => return Err(ObjectError::InvalidKind),
        };
        if index >= array_length {
            return Err(ObjectError::InvalidIndex);
        }
        let bigint_str = match self.object(bigint_handle)? {
            HeapValue::BigInt { value } => value.to_string(),
            _ => return Err(ObjectError::InvalidKind),
        };
        let elem_size = kind.element_size();
        let byte_index = byte_offset + index * elem_size;
        let buffer_data = self.array_buffer_or_shared_data_mut(viewed_buffer)?;
        if byte_index + elem_size > buffer_data.len() {
            return Err(ObjectError::InvalidIndex);
        }
        let dest = &mut buffer_data[byte_index..byte_index + elem_size];
        match kind {
            TypedArrayKind::BigInt64 => {
                let n: i64 = bigint_str.parse().unwrap_or(0);
                dest.copy_from_slice(&n.to_le_bytes());
            }
            TypedArrayKind::BigUint64 => {
                let n: u64 = bigint_str.parse().unwrap_or(0);
                dest.copy_from_slice(&n.to_le_bytes());
            }
            _ => return Err(ObjectError::InvalidKind),
        }
        Ok(())
    }

    /// Returns whether this handle is a TypedArray.
    pub fn is_typed_array(&self, handle: ObjectHandle) -> bool {
        matches!(self.object(handle), Ok(HeapValue::TypedArray { .. }))
    }

    // ── RegExp ───────────────────────────────────────────────────────

    /// Allocates a RegExp object with the given pattern and flags.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-regexp-objects>
    pub fn alloc_regexp(
        &mut self,
        pattern: &str,
        flags: &str,
        prototype: Option<ObjectHandle>,
    ) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::RegExp {
            prototype,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            pattern: pattern.into(),
            flags: flags.into(),
        });
        ObjectHandle(h.0)
    }

    /// Returns the pattern string of a RegExp object.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-regexp-instances>
    pub fn regexp_pattern(&self, handle: ObjectHandle) -> Result<&str, ObjectError> {
        match self.object(handle)? {
            HeapValue::RegExp { pattern, .. } => Ok(pattern),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the flags string of a RegExp object.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-properties-of-regexp-instances>
    pub fn regexp_flags(&self, handle: ObjectHandle) -> Result<&str, ObjectError> {
        match self.object(handle)? {
            HeapValue::RegExp { flags, .. } => Ok(flags),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Mutates the pattern and flags of a RegExp object in place.
    /// Used by the deprecated `RegExp.prototype.compile` (Annex B §B.2.4).
    pub fn set_regexp_pattern_flags(
        &mut self,
        handle: ObjectHandle,
        new_pattern: &str,
        new_flags: &str,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::RegExp { pattern, flags, .. } => {
                *pattern = new_pattern.into();
                *flags = new_flags.into();
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Map.prototype.get — returns the value for the key, or undefined.
    pub fn map_get(
        &self,
        handle: ObjectHandle,
        key: RegisterValue,
    ) -> Result<RegisterValue, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => {
                for entry in entries.iter().flatten() {
                    if svz(&self.heap, entry.0, key) {
                        return Ok(entry.1);
                    }
                }
                Ok(RegisterValue::undefined())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Map.prototype.has — returns true if the key exists.
    pub fn map_has(&self, handle: ObjectHandle, key: RegisterValue) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => {
                Ok(entries.iter().flatten().any(|e| svz(&self.heap, e.0, key)))
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Map.prototype.set — inserts or updates a key-value pair.
    pub fn map_set(
        &mut self,
        handle: ObjectHandle,
        key: RegisterValue,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        let normalized_key = normalize_zero(key);
        if let Some(idx) = self.map_find_index(handle, normalized_key)? {
            match self.object_mut(handle)? {
                HeapValue::Map { entries, .. } => {
                    entries[idx] = Some((normalized_key, value));
                }
                _ => return Err(ObjectError::InvalidKind),
            }
        } else {
            match self.object_mut(handle)? {
                HeapValue::Map { entries, .. } => {
                    entries.push(Some((normalized_key, value)));
                }
                _ => return Err(ObjectError::InvalidKind),
            }
        }
        Ok(())
    }

    /// Map.prototype.delete — removes a key and returns whether it existed.
    pub fn map_delete(
        &mut self,
        handle: ObjectHandle,
        key: RegisterValue,
    ) -> Result<bool, ObjectError> {
        if let Some(idx) = self.map_find_index(handle, key)? {
            match self.object_mut(handle)? {
                HeapValue::Map { entries, .. } => {
                    entries[idx] = None;
                    return Ok(true);
                }
                _ => return Err(ObjectError::InvalidKind),
            }
        }
        Ok(false)
    }

    /// Map.prototype.size — returns the number of live entries.
    pub fn map_size(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => Ok(entries.iter().flatten().count()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Map.prototype.clear — removes all entries.
    pub fn map_clear(&mut self, handle: ObjectHandle) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Map { entries, .. } => {
                entries.clear();
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Collect all live Map entries as (key, value) pairs.
    pub fn map_entries(
        &self,
        handle: ObjectHandle,
    ) -> Result<Vec<(RegisterValue, RegisterValue)>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => Ok(entries.iter().filter_map(|e| *e).collect()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Set.prototype.has — returns true if the value exists.
    pub fn set_has(&self, handle: ObjectHandle, value: RegisterValue) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => {
                Ok(entries.iter().flatten().any(|e| svz(&self.heap, *e, value)))
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Set.prototype.add — inserts a value if not present.
    pub fn set_add(
        &mut self,
        handle: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        let normalized = normalize_zero(value);
        if self.set_find_index(handle, normalized)?.is_some() {
            return Ok(());
        }
        match self.object_mut(handle)? {
            HeapValue::Set { entries, .. } => {
                entries.push(Some(normalized));
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Set.prototype.delete — removes a value and returns whether it existed.
    pub fn set_delete(
        &mut self,
        handle: ObjectHandle,
        value: RegisterValue,
    ) -> Result<bool, ObjectError> {
        if let Some(idx) = self.set_find_index(handle, value)? {
            match self.object_mut(handle)? {
                HeapValue::Set { entries, .. } => {
                    entries[idx] = None;
                    return Ok(true);
                }
                _ => return Err(ObjectError::InvalidKind),
            }
        }
        Ok(false)
    }

    /// Set.prototype.size — returns the number of live entries.
    pub fn set_size(&self, handle: ObjectHandle) -> Result<usize, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => Ok(entries.iter().flatten().count()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Set.prototype.clear — removes all entries.
    pub fn set_clear(&mut self, handle: ObjectHandle) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Set { entries, .. } => {
                entries.clear();
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Collect all live Set values.
    pub fn set_values(&self, handle: ObjectHandle) -> Result<Vec<RegisterValue>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => Ok(entries.iter().filter_map(|e| *e).collect()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Allocates a WeakMap object.
    /// Spec: <https://tc39.es/ecma262/#sec-weakmap-constructor>
    pub fn alloc_weakmap(&mut self, prototype: Option<ObjectHandle>) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::WeakMap {
            prototype,
            entries: std::collections::HashMap::new(),
        });
        ObjectHandle(h.0)
    }

    /// Allocates a WeakSet object.
    /// Spec: <https://tc39.es/ecma262/#sec-weakset-constructor>
    pub fn alloc_weakset(&mut self, prototype: Option<ObjectHandle>) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::WeakSet {
            prototype,
            entries: std::collections::HashSet::new(),
        });
        ObjectHandle(h.0)
    }

    /// WeakMap.prototype.get(key)
    /// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.get>
    pub fn weakmap_get(
        &self,
        handle: ObjectHandle,
        key: u32,
    ) -> Result<Option<RegisterValue>, ObjectError> {
        match self.object(handle)? {
            HeapValue::WeakMap { entries, .. } => Ok(entries.get(&key).copied()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// WeakMap.prototype.set(key, value)
    /// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.set>
    pub fn weakmap_set(
        &mut self,
        handle: ObjectHandle,
        key: u32,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::WeakMap { entries, .. } => {
                entries.insert(key, value);
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// WeakMap.prototype.has(key)
    /// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.has>
    pub fn weakmap_has(&self, handle: ObjectHandle, key: u32) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::WeakMap { entries, .. } => Ok(entries.contains_key(&key)),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// WeakMap.prototype.delete(key)
    /// Spec: <https://tc39.es/ecma262/#sec-weakmap.prototype.delete>
    pub fn weakmap_delete(&mut self, handle: ObjectHandle, key: u32) -> Result<bool, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::WeakMap { entries, .. } => Ok(entries.remove(&key).is_some()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// WeakSet.prototype.add(value)
    /// Spec: <https://tc39.es/ecma262/#sec-weakset.prototype.add>
    pub fn weakset_add(&mut self, handle: ObjectHandle, key: u32) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::WeakSet { entries, .. } => {
                entries.insert(key);
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// WeakSet.prototype.has(value)
    /// Spec: <https://tc39.es/ecma262/#sec-weakset.prototype.has>
    pub fn weakset_has(&self, handle: ObjectHandle, key: u32) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::WeakSet { entries, .. } => Ok(entries.contains(&key)),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// WeakSet.prototype.delete(value)
    /// Spec: <https://tc39.es/ecma262/#sec-weakset.prototype.delete>
    pub fn weakset_delete(&mut self, handle: ObjectHandle, key: u32) -> Result<bool, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::WeakSet { entries, .. } => Ok(entries.remove(&key)),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Removes dead entries from a WeakMap. Returns values of surviving entries for ephemeron tracing.
    pub fn weakmap_clear_dead_and_get_live_values(
        &mut self,
        handle: ObjectHandle,
        is_live: &dyn Fn(u32) -> bool,
    ) -> Result<Vec<RegisterValue>, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::WeakMap { entries, .. } => {
                entries.retain(|key, _| is_live(*key));
                Ok(entries.values().copied().collect())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Removes dead entries from a WeakSet.
    pub fn weakset_clear_dead(
        &mut self,
        handle: ObjectHandle,
        is_live: &dyn Fn(u32) -> bool,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::WeakSet { entries, .. } => {
                entries.retain(|key| is_live(*key));
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    // ─── WeakRef Objects (§26.1) ──────────────────────────────────────

    /// Allocates a WeakRef object holding a weak reference to `target`.
    /// Spec: <https://tc39.es/ecma262/#sec-weak-ref-target>
    pub fn alloc_weakref(
        &mut self,
        prototype: Option<ObjectHandle>,
        target: ObjectHandle,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::WeakRef {
            prototype,
            target: Some(target.0),
        });
        ObjectHandle(h.0)
    }

    /// WeakRef.prototype.deref() — returns the target or undefined.
    /// Spec: <https://tc39.es/ecma262/#sec-weak-ref.prototype.deref>
    pub fn weakref_deref(&self, handle: ObjectHandle) -> Result<Option<ObjectHandle>, ObjectError> {
        match self.object(handle)? {
            HeapValue::WeakRef { target, .. } => Ok(target.map(ObjectHandle)),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Clears dead WeakRef targets during GC.
    pub fn weakref_clear_dead(
        &mut self,
        handle: ObjectHandle,
        is_live: &dyn Fn(u32) -> bool,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::WeakRef { target, .. } => {
                if let Some(t) = *target
                    && !is_live(t)
                {
                    *target = None;
                }
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    // ─── FinalizationRegistry Objects (§26.2) ──────────────────────────

    /// Allocates a FinalizationRegistry object.
    /// Spec: <https://tc39.es/ecma262/#sec-finalization-registry-cleanup-callback>
    pub fn alloc_finalization_registry(
        &mut self,
        prototype: Option<ObjectHandle>,
        cleanup_callback: ObjectHandle,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::FinalizationRegistry {
            prototype,
            cleanup_callback,
            cells: Vec::new(),
        });
        ObjectHandle(h.0)
    }

    /// FinalizationRegistry.prototype.register(target, heldValue, unregisterToken)
    /// Spec: <https://tc39.es/ecma262/#sec-finalization-registry.prototype.register>
    pub fn finalization_registry_register(
        &mut self,
        handle: ObjectHandle,
        target: ObjectHandle,
        held_value: RegisterValue,
        unregister_token: Option<ObjectHandle>,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::FinalizationRegistry { cells, .. } => {
                cells.push(FinalizationCell {
                    target: Some(target.0),
                    held_value,
                    unregister_token: unregister_token.map(|t| t.0),
                });
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// FinalizationRegistry.prototype.unregister(unregisterToken)
    /// Spec: <https://tc39.es/ecma262/#sec-finalization-registry.prototype.unregister>
    ///
    /// Returns `true` if any cells were removed.
    pub fn finalization_registry_unregister(
        &mut self,
        handle: ObjectHandle,
        token: ObjectHandle,
    ) -> Result<bool, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::FinalizationRegistry { cells, .. } => {
                let before = cells.len();
                cells.retain(|cell| cell.unregister_token != Some(token.0));
                Ok(cells.len() != before)
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the cleanup callback for a FinalizationRegistry.
    pub fn finalization_registry_callback(
        &self,
        handle: ObjectHandle,
    ) -> Result<ObjectHandle, ObjectError> {
        match self.object(handle)? {
            HeapValue::FinalizationRegistry {
                cleanup_callback, ..
            } => Ok(*cleanup_callback),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// During GC: marks cells whose targets are dead and returns their held values
    /// for cleanup callback invocation. Removes those cells from the registry.
    pub fn finalization_registry_clear_dead(
        &mut self,
        handle: ObjectHandle,
        is_live: &dyn Fn(u32) -> bool,
    ) -> Result<Vec<RegisterValue>, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::FinalizationRegistry { cells, .. } => {
                let mut cleanup_values = Vec::new();
                cells.retain(|cell| {
                    if let Some(t) = cell.target
                        && !is_live(t)
                    {
                        cleanup_values.push(cell.held_value);
                        return false;
                    }
                    true
                });
                Ok(cleanup_values)
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    // ─── Generator Objects (§27.5) ──────────────────────────────────────

    /// Allocates a generator object in `SuspendedStart` state.
    /// Spec: <https://tc39.es/ecma262/#sec-generatorstart>
    pub fn alloc_generator(
        &mut self,
        prototype: Option<ObjectHandle>,
        module: Module,
        function_index: FunctionIndex,
        closure_handle: Option<ObjectHandle>,
        arguments: Vec<RegisterValue>,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::Generator {
            prototype,
            state: GeneratorState::SuspendedStart,
            module,
            function_index,
            closure_handle,
            arguments,
            registers: None,
            resume_pc: 0,
            resume_register: 0,
            delegation_iterator: None,
        });
        ObjectHandle(h.0)
    }

    /// Returns the current generator state.
    pub fn generator_state(&self, handle: ObjectHandle) -> Result<GeneratorState, ObjectError> {
        match self.object(handle)? {
            HeapValue::Generator { state, .. } => Ok(*state),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Sets the generator state.
    pub fn set_generator_state(
        &mut self,
        handle: ObjectHandle,
        new_state: GeneratorState,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Generator { state, .. } => {
                *state = new_state;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Saves the register window and PC into the generator for suspension.
    pub fn generator_save_state(
        &mut self,
        handle: ObjectHandle,
        saved_registers: Box<[RegisterValue]>,
        pc: ProgramCounter,
        resume_reg: u16,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Generator {
                registers,
                resume_pc,
                resume_register,
                state,
                ..
            } => {
                *registers = Some(saved_registers);
                *resume_pc = pc;
                *resume_register = resume_reg;
                *state = GeneratorState::SuspendedYield;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Takes the saved state from a generator, transitioning it to `Executing`.
    /// Returns `(module, function_index, closure_handle, arguments, registers, resume_pc, resume_register)`.
    #[allow(clippy::type_complexity)]
    pub fn generator_take_state(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<
        (
            Module,
            FunctionIndex,
            Option<ObjectHandle>,
            Vec<RegisterValue>,
            Option<Box<[RegisterValue]>>,
            ProgramCounter,
            u16,
        ),
        ObjectError,
    > {
        match self.object_mut(handle)? {
            HeapValue::Generator {
                module,
                function_index,
                closure_handle,
                arguments,
                registers,
                resume_pc,
                resume_register,
                state,
                ..
            } => {
                *state = GeneratorState::Executing;
                Ok((
                    module.clone(),
                    *function_index,
                    *closure_handle,
                    std::mem::take(arguments),
                    registers.take(),
                    *resume_pc,
                    *resume_register,
                ))
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    // ─── yield* Delegation (§14.4.4) ─────────────────────────────────────

    /// Returns the active delegation iterator for a generator (sync or async).
    /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>
    pub fn generator_delegation_iterator(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Generator {
                delegation_iterator,
                ..
            }
            | HeapValue::AsyncGenerator {
                delegation_iterator,
                ..
            } => Ok(*delegation_iterator),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Sets the active delegation iterator for a generator.
    pub fn set_generator_delegation_iterator(
        &mut self,
        handle: ObjectHandle,
        iterator: Option<ObjectHandle>,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Generator {
                delegation_iterator,
                ..
            }
            | HeapValue::AsyncGenerator {
                delegation_iterator,
                ..
            } => {
                *delegation_iterator = iterator;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    // ─── Async Generator Objects (§27.6) ─────────────────────────────────

    /// Allocates an async generator object in `SuspendedStart` state.
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorstart>
    pub fn alloc_async_generator(
        &mut self,
        prototype: Option<ObjectHandle>,
        module: Module,
        function_index: FunctionIndex,
        closure_handle: Option<ObjectHandle>,
        arguments: Vec<RegisterValue>,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::AsyncGenerator {
            prototype,
            state: GeneratorState::SuspendedStart,
            module,
            function_index,
            closure_handle,
            arguments,
            registers: None,
            resume_pc: 0,
            resume_register: 0,
            queue: Vec::new(),
            delegation_iterator: None,
        });
        ObjectHandle(h.0)
    }

    /// Returns the current async generator state.
    pub fn async_generator_state(
        &self,
        handle: ObjectHandle,
    ) -> Result<GeneratorState, ObjectError> {
        match self.object(handle)? {
            HeapValue::AsyncGenerator { state, .. } => Ok(*state),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Sets the async generator state.
    pub fn set_async_generator_state(
        &mut self,
        handle: ObjectHandle,
        new_state: GeneratorState,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::AsyncGenerator { state, .. } => {
                *state = new_state;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Pushes a request into the async generator's queue.
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorenqueue>
    pub fn async_generator_enqueue(
        &mut self,
        handle: ObjectHandle,
        request: AsyncGeneratorRequest,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::AsyncGenerator { queue, .. } => {
                queue.push(request);
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Peeks at the front request in the queue (without removing it).
    pub fn async_generator_peek_request(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<AsyncGeneratorRequest>, ObjectError> {
        match self.object(handle)? {
            HeapValue::AsyncGenerator { queue, .. } => Ok(queue.first().copied()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Removes the front request from the queue.
    pub fn async_generator_dequeue(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Option<AsyncGeneratorRequest>, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::AsyncGenerator { queue, .. } => {
                if queue.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(queue.remove(0)))
                }
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns whether the async generator's request queue is empty.
    pub fn async_generator_queue_is_empty(
        &self,
        handle: ObjectHandle,
    ) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::AsyncGenerator { queue, .. } => Ok(queue.is_empty()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Saves the register window and PC into the async generator for suspension.
    pub fn async_generator_save_state(
        &mut self,
        handle: ObjectHandle,
        saved_registers: Box<[RegisterValue]>,
        pc: ProgramCounter,
        resume_reg: u16,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::AsyncGenerator {
                registers,
                resume_pc,
                resume_register,
                state,
                ..
            } => {
                *registers = Some(saved_registers);
                *resume_pc = pc;
                *resume_register = resume_reg;
                *state = GeneratorState::SuspendedYield;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Takes the saved state from an async generator, transitioning it to `Executing`.
    #[allow(clippy::type_complexity)]
    pub fn async_generator_take_state(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<
        (
            Module,
            FunctionIndex,
            Option<ObjectHandle>,
            Vec<RegisterValue>,
            Option<Box<[RegisterValue]>>,
            ProgramCounter,
            u16,
        ),
        ObjectError,
    > {
        match self.object_mut(handle)? {
            HeapValue::AsyncGenerator {
                module,
                function_index,
                closure_handle,
                arguments,
                registers,
                resume_pc,
                resume_register,
                state,
                ..
            } => {
                *state = GeneratorState::Executing;
                Ok((
                    module.clone(),
                    *function_index,
                    *closure_handle,
                    std::mem::take(arguments),
                    registers.take(),
                    *resume_pc,
                    *resume_register,
                ))
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Allocates a string value from UTF-8 input.
    ///
    /// The UTF-8 input is converted to WTF-16 (UTF-16 code units) for storage.
    /// For strings that may contain lone surrogates, use [`alloc_js_string`].
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let s: Box<str> = value.into();
        let js = JsString::from_str(&s);
        let h = self.heap.alloc(HeapValue::String {
            prototype: None,
            value: js,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a string value from a `JsString` (WTF-16).
    ///
    /// Preserves lone surrogates as-is — no validation or replacement.
    pub fn alloc_js_string(&mut self, value: JsString) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::String {
            prototype: None,
            value,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a heap-stored BigInt value.
    ///
    /// §6.1.6.2 The BigInt Type
    /// <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    pub fn alloc_bigint(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::BigInt {
            value: value.into(),
        });
        ObjectHandle(h.0)
    }

    /// Returns the decimal string value of a BigInt heap object.
    pub fn bigint_value(&self, handle: ObjectHandle) -> Result<Option<&str>, ObjectError> {
        match self.object(handle)? {
            HeapValue::BigInt { value } => Ok(Some(value)),
            _ => Ok(None),
        }
    }

    /// Allocates a heap-stored stack frame snapshot bag.
    ///
    /// V8 extension — used to back the lazily formatted
    /// `Error.prototype.stack` accessor.
    pub fn alloc_error_stack_frames(
        &mut self,
        frames: Vec<crate::stack_frame::StackFrameInfo>,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::ErrorStackFrames { frames });
        ObjectHandle(h.0)
    }

    /// Returns the captured stack frames stored on an `ErrorStackFrames`
    /// heap value.
    pub fn error_stack_frames(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<&[crate::stack_frame::StackFrameInfo]>, ObjectError> {
        match self.object(handle)? {
            HeapValue::ErrorStackFrames { frames } => Ok(Some(frames)),
            _ => Ok(None),
        }
    }

    /// Allocates a mutable upvalue cell.
    pub fn alloc_upvalue(&mut self, value: RegisterValue) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::UpvalueCell { value });
        ObjectHandle(h.0)
    }

    /// Allocates a closure object with captured upvalue cells and function kind flags.
    pub fn alloc_closure(
        &mut self,
        module: Module,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
        flags: ClosureFlags,
        realm: crate::realm::RealmId,
    ) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::Closure {
            prototype: None,
            extensible: true,
            flags,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            module,
            callee,
            upvalues,
            field_initializer: None,
            class_id: 0,
            private_methods: Vec::new(),
            private_elements: Vec::new(),
            realm,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a host-callable native function object.
    pub fn alloc_host_function(
        &mut self,
        function: HostFunctionId,
        realm: crate::realm::RealmId,
    ) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::HostFunction {
            function,
            prototype: None,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            realm,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a new pending promise with an optional prototype.
    pub fn alloc_promise(&mut self) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::Promise {
            prototype: None,
            promise: crate::promise::JsPromise::new(),
        });
        ObjectHandle(h.0)
    }

    /// Allocates a new pending promise with the given prototype.
    pub fn alloc_promise_with_proto(&mut self, prototype: ObjectHandle) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::Promise {
            prototype: Some(prototype),
            promise: crate::promise::JsPromise::new(),
        });
        ObjectHandle(h.0)
    }

    /// Reads a reference to a JsPromise stored in the heap.
    pub fn get_promise(&self, handle: ObjectHandle) -> Option<&crate::promise::JsPromise> {
        match self.object(handle).ok()? {
            HeapValue::Promise { promise, .. } => Some(promise),
            _ => None,
        }
    }

    /// Reads a mutable reference to a JsPromise.
    pub fn get_promise_mut(
        &mut self,
        handle: ObjectHandle,
    ) -> Option<&mut crate::promise::JsPromise> {
        match self.object_mut(handle).ok()? {
            HeapValue::Promise { promise, .. } => Some(promise),
            _ => None,
        }
    }

    /// Allocates a promise capability resolve or reject function.
    /// ES2024 §27.2.1.5 NewPromiseCapability — creates the internal resolve/reject functions.
    /// Spec: <https://tc39.es/ecma262/#sec-newpromisecapability>
    pub fn alloc_promise_capability_function(
        &mut self,
        promise: ObjectHandle,
        kind: crate::promise::ReactionKind,
    ) -> ObjectHandle {
        let h = self
            .heap
            .alloc(HeapValue::PromiseCapabilityFunction { promise, kind });
        ObjectHandle(h.0)
    }

    /// Creates a full promise capability: promise + resolve function + reject function.
    /// Returns (promise, resolve, reject) triple.
    /// ES2024 §27.2.1.5 NewPromiseCapability
    /// Spec: <https://tc39.es/ecma262/#sec-newpromisecapability>
    pub fn alloc_promise_capability(&mut self) -> crate::promise::PromiseCapability {
        let promise = self.alloc_promise();
        let resolve =
            self.alloc_promise_capability_function(promise, crate::promise::ReactionKind::Fulfill);
        let reject =
            self.alloc_promise_capability_function(promise, crate::promise::ReactionKind::Reject);
        crate::promise::PromiseCapability {
            promise,
            resolve,
            reject,
        }
    }

    /// Allocates a per-element function for Promise.all/allSettled/any combinators.
    pub fn alloc_promise_combinator_element(
        &mut self,
        combinator_kind: crate::promise::PromiseCombinatorKind,
        index: u32,
        result_array: ObjectHandle,
        remaining_counter: ObjectHandle,
        result_capability: crate::promise::PromiseCapability,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::PromiseCombinatorElement {
            combinator_kind,
            index,
            result_array,
            remaining_counter,
            result_capability,
            already_called: false,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a finally wrapper function.
    pub fn alloc_promise_finally_function(
        &mut self,
        on_finally: ObjectHandle,
        constructor: ObjectHandle,
        kind: crate::promise::PromiseFinallyKind,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::PromiseFinallyFunction {
            on_finally,
            constructor,
            kind,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a value thunk for finally chaining.
    pub fn alloc_promise_value_thunk(
        &mut self,
        value: crate::value::RegisterValue,
        kind: crate::promise::PromiseFinallyKind,
    ) -> ObjectHandle {
        let h = self
            .heap
            .alloc(HeapValue::PromiseValueThunk { value, kind });
        ObjectHandle(h.0)
    }

    /// Allocates a new Proxy exotic object.
    /// Spec: <https://tc39.es/ecma262/#sec-proxycreate>
    pub fn alloc_proxy(&mut self, target: ObjectHandle, handler: ObjectHandle) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::Proxy {
            target,
            handler,
            revoked: false,
        });
        ObjectHandle(h.0)
    }

    /// Returns `true` if the handle points to a Proxy exotic object.
    pub fn is_proxy(&self, handle: ObjectHandle) -> bool {
        matches!(self.object(handle), Ok(HeapValue::Proxy { .. }))
    }

    /// Returns the [[ProxyTarget]] and [[ProxyHandler]] for a proxy.
    /// Returns `Err` if revoked or not a proxy.
    pub fn proxy_parts(
        &self,
        handle: ObjectHandle,
    ) -> Result<(ObjectHandle, ObjectHandle), ObjectError> {
        match self.object(handle)? {
            HeapValue::Proxy {
                target,
                handler,
                revoked: false,
            } => Ok((*target, *handler)),
            HeapValue::Proxy { revoked: true, .. } => Err(ObjectError::InvalidKind), // caller must check
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns `true` if the proxy has been revoked.
    pub fn is_proxy_revoked(&self, handle: ObjectHandle) -> bool {
        matches!(
            self.object(handle),
            Ok(HeapValue::Proxy { revoked: true, .. })
        )
    }

    /// Revokes a proxy object, making all trap operations throw TypeError.
    pub fn revoke_proxy(&mut self, handle: ObjectHandle) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Proxy { revoked, .. } => {
                *revoked = true;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the target promise and kind for a PromiseCapabilityFunction.
    pub fn promise_capability_function_info(
        &self,
        handle: ObjectHandle,
    ) -> Option<(ObjectHandle, crate::promise::ReactionKind)> {
        match self.object(handle).ok()? {
            HeapValue::PromiseCapabilityFunction { promise, kind } => Some((*promise, *kind)),
            _ => None,
        }
    }

    /// Returns combinator element info: (kind, index, result_array, counter, capability, already_called).
    pub fn promise_combinator_element_info(
        &self,
        handle: ObjectHandle,
    ) -> Option<(
        crate::promise::PromiseCombinatorKind,
        u32,
        ObjectHandle,
        ObjectHandle,
        crate::promise::PromiseCapability,
        bool,
    )> {
        match self.object(handle).ok()? {
            HeapValue::PromiseCombinatorElement {
                combinator_kind,
                index,
                result_array,
                remaining_counter,
                result_capability,
                already_called,
            } => Some((
                *combinator_kind,
                *index,
                *result_array,
                *remaining_counter,
                *result_capability,
                *already_called,
            )),
            _ => None,
        }
    }

    /// Sets the already_called flag on a combinator element.
    pub fn set_combinator_element_called(&mut self, handle: ObjectHandle) {
        if let Ok(HeapValue::PromiseCombinatorElement { already_called, .. }) =
            self.object_mut(handle)
        {
            *already_called = true;
        }
    }

    /// Returns finally function info: (on_finally, constructor, kind).
    pub fn promise_finally_function_info(
        &self,
        handle: ObjectHandle,
    ) -> Option<(
        ObjectHandle,
        ObjectHandle,
        crate::promise::PromiseFinallyKind,
    )> {
        match self.object(handle).ok()? {
            HeapValue::PromiseFinallyFunction {
                on_finally,
                constructor,
                kind,
            } => Some((*on_finally, *constructor, *kind)),
            _ => None,
        }
    }

    /// Returns value thunk info: (value, kind).
    pub fn promise_value_thunk_info(
        &self,
        handle: ObjectHandle,
    ) -> Option<(
        crate::value::RegisterValue,
        crate::promise::PromiseFinallyKind,
    )> {
        match self.object(handle).ok()? {
            HeapValue::PromiseValueThunk { value, kind } => Some((*value, *kind)),
            _ => None,
        }
    }

    /// Returns the heap-value kind for the given handle.
    pub fn kind(&self, handle: ObjectHandle) -> Result<HeapValueKind, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. } => Ok(HeapValueKind::Object),
            HeapValue::NativeObject { .. } => Ok(HeapValueKind::Object),
            HeapValue::HostFunction { .. } => Ok(HeapValueKind::HostFunction),
            HeapValue::Array { .. } => Ok(HeapValueKind::Array),
            HeapValue::String { .. } => Ok(HeapValueKind::String),
            HeapValue::Closure { .. } => Ok(HeapValueKind::Closure),
            HeapValue::BoundFunction { .. } => Ok(HeapValueKind::BoundFunction),
            HeapValue::UpvalueCell { .. } => Ok(HeapValueKind::UpvalueCell),
            HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. } => Ok(HeapValueKind::Iterator),
            HeapValue::Promise { .. } => Ok(HeapValueKind::Promise),
            HeapValue::PromiseCapabilityFunction { .. } => {
                Ok(HeapValueKind::PromiseCapabilityFunction)
            }
            HeapValue::PromiseCombinatorElement { .. } => {
                Ok(HeapValueKind::PromiseCombinatorElement)
            }
            HeapValue::PromiseFinallyFunction { .. } => Ok(HeapValueKind::PromiseFinallyFunction),
            HeapValue::PromiseValueThunk { .. } => Ok(HeapValueKind::PromiseValueThunk),
            HeapValue::Map { .. } => Ok(HeapValueKind::Map),
            HeapValue::Set { .. } => Ok(HeapValueKind::Set),
            HeapValue::MapIterator { .. } => Ok(HeapValueKind::MapIterator),
            HeapValue::SetIterator { .. } => Ok(HeapValueKind::SetIterator),
            HeapValue::WeakMap { .. } => Ok(HeapValueKind::WeakMap),
            HeapValue::WeakSet { .. } => Ok(HeapValueKind::WeakSet),
            HeapValue::WeakRef { .. } => Ok(HeapValueKind::WeakRef),
            HeapValue::FinalizationRegistry { .. } => Ok(HeapValueKind::FinalizationRegistry),
            HeapValue::Generator { .. } => Ok(HeapValueKind::Generator),
            HeapValue::AsyncGenerator { .. } => Ok(HeapValueKind::AsyncGenerator),
            HeapValue::ArrayBuffer { .. } => Ok(HeapValueKind::ArrayBuffer),
            HeapValue::SharedArrayBuffer { .. } => Ok(HeapValueKind::SharedArrayBuffer),
            HeapValue::RegExp { .. } => Ok(HeapValueKind::RegExp),
            HeapValue::DataView { .. } => Ok(HeapValueKind::DataView),
            HeapValue::TypedArray { .. } => Ok(HeapValueKind::TypedArray),
            HeapValue::Proxy { .. } => Ok(HeapValueKind::Proxy),
            HeapValue::BigInt { .. } => Ok(HeapValueKind::BigInt),
            HeapValue::ErrorStackFrames { .. } => Ok(HeapValueKind::ErrorStackFrames),
        }
    }

    /// Returns the direct prototype link for the given heap value.
    pub fn get_prototype(&self, handle: ObjectHandle) -> Result<Option<ObjectHandle>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { prototype, .. }
            | HeapValue::NativeObject { prototype, .. }
            | HeapValue::Array { prototype, .. }
            | HeapValue::String { prototype, .. }
            | HeapValue::Closure { prototype, .. }
            | HeapValue::HostFunction { prototype, .. }
            | HeapValue::BoundFunction { prototype, .. }
            | HeapValue::Map { prototype, .. }
            | HeapValue::Set { prototype, .. }
            | HeapValue::ArrayIterator { prototype, .. }
            | HeapValue::StringIterator { prototype, .. }
            | HeapValue::MapIterator { prototype, .. }
            | HeapValue::SetIterator { prototype, .. }
            | HeapValue::WeakMap { prototype, .. }
            | HeapValue::WeakSet { prototype, .. }
            | HeapValue::WeakRef { prototype, .. }
            | HeapValue::FinalizationRegistry { prototype, .. }
            | HeapValue::Generator { prototype, .. }
            | HeapValue::AsyncGenerator { prototype, .. }
            | HeapValue::Promise { prototype, .. }
            | HeapValue::ArrayBuffer { prototype, .. }
            | HeapValue::SharedArrayBuffer { prototype, .. }
            | HeapValue::DataView { prototype, .. }
            | HeapValue::TypedArray { prototype, .. }
            | HeapValue::RegExp { prototype, .. } => Ok(*prototype),
            // Proxy: delegate to target's prototype (trap invocation is at interpreter level).
            HeapValue::Proxy { target, .. } => self.get_prototype(*target),
            HeapValue::UpvalueCell { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Updates the direct prototype link for the given heap value.
    /// Sets the prototype of an object, with cycle detection.
    /// Returns `Ok(true)` if the prototype was set, `Ok(false)` if rejected
    /// (cycle detected or depth exceeded).
    pub fn set_prototype(
        &mut self,
        handle: ObjectHandle,
        prototype: Option<ObjectHandle>,
    ) -> Result<bool, ObjectError> {
        let current = self.get_prototype(handle)?;
        if current == prototype {
            return Ok(true);
        }
        let is_string_value = matches!(self.object(handle)?, HeapValue::String { .. });
        if !is_string_value && !self.is_extensible(handle)? {
            return Ok(false);
        }

        // Cycle detection: walk from new_prototype upward and check if we
        // encounter `handle`.  This matches V8's OrdinarySetPrototypeOf and
        // the mature VM's implementation.
        if let Some(new_proto) = prototype {
            self.object(new_proto)?;
            let mut current = Some(new_proto);
            let mut depth = 0;
            while let Some(p) = current {
                depth += 1;
                if depth > MAX_SET_PROTOTYPE_DEPTH {
                    return Ok(false);
                }
                if p == handle {
                    return Ok(false); // cycle detected
                }
                current = self.property_traversal_prototype(p)?;
            }
        }

        match self.object_mut(handle)? {
            HeapValue::Object {
                prototype: slot, ..
            }
            | HeapValue::NativeObject {
                prototype: slot, ..
            }
            | HeapValue::Array {
                prototype: slot, ..
            }
            | HeapValue::String {
                prototype: slot, ..
            }
            | HeapValue::Closure {
                prototype: slot, ..
            }
            | HeapValue::HostFunction {
                prototype: slot, ..
            }
            | HeapValue::BoundFunction {
                prototype: slot, ..
            }
            | HeapValue::Map {
                prototype: slot, ..
            }
            | HeapValue::Set {
                prototype: slot, ..
            }
            | HeapValue::ArrayIterator {
                prototype: slot, ..
            }
            | HeapValue::StringIterator {
                prototype: slot, ..
            }
            | HeapValue::MapIterator {
                prototype: slot, ..
            }
            | HeapValue::SetIterator {
                prototype: slot, ..
            }
            | HeapValue::WeakMap {
                prototype: slot, ..
            }
            | HeapValue::WeakSet {
                prototype: slot, ..
            }
            | HeapValue::WeakRef {
                prototype: slot, ..
            }
            | HeapValue::FinalizationRegistry {
                prototype: slot, ..
            }
            | HeapValue::Generator {
                prototype: slot, ..
            }
            | HeapValue::AsyncGenerator {
                prototype: slot, ..
            }
            | HeapValue::Promise {
                prototype: slot, ..
            }
            | HeapValue::ArrayBuffer {
                prototype: slot, ..
            }
            | HeapValue::SharedArrayBuffer {
                prototype: slot, ..
            }
            | HeapValue::DataView {
                prototype: slot, ..
            }
            | HeapValue::TypedArray {
                prototype: slot, ..
            }
            | HeapValue::RegExp {
                prototype: slot, ..
            } => {
                *slot = prototype;
                Ok(true)
            }
            HeapValue::Proxy { target, .. } => {
                let target = *target;
                self.set_prototype(target, prototype)
            }
            HeapValue::UpvalueCell { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns a built-in fast-path property value when one exists.
    pub fn get_builtin_property(
        &self,
        handle: ObjectHandle,
        property_name: &str,
    ) -> Result<Option<RegisterValue>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => Ok(None),
            HeapValue::Closure { .. } => Ok(None),
            HeapValue::Array { elements, .. } if property_name == "length" => Ok(Some(
                RegisterValue::from_i32(i32::try_from(elements.len()).unwrap_or(i32::MAX)),
            )),
            HeapValue::String { value, .. } if property_name == "length" => Ok(Some(
                RegisterValue::from_i32(i32::try_from(value.len()).unwrap_or(i32::MAX)),
            )),
            HeapValue::Array { .. } | HeapValue::String { .. } => Ok(None),
        }
    }

    /// Compares two register values with the current strict-equality semantics.
    pub fn strict_eq(&self, lhs: RegisterValue, rhs: RegisterValue) -> Result<bool, ObjectError> {
        crate::abstract_ops::is_strictly_equal(self, lhs, rhs)
    }

    /// ES2024 §7.2.9 SameValue(x, y).
    pub fn same_value(&self, lhs: RegisterValue, rhs: RegisterValue) -> Result<bool, ObjectError> {
        crate::abstract_ops::same_value(self, lhs, rhs)
    }

    /// ES2024 §7.2.10 SameValueZero(x, y).
    pub fn same_value_zero(
        &self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<bool, ObjectError> {
        crate::abstract_ops::same_value_zero(self, lhs, rhs)
    }

    /// Loads an indexed element from an array or string.
    pub fn get_index(
        &mut self,
        handle: ObjectHandle,
        index: usize,
    ) -> Result<Option<RegisterValue>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Array {
                elements,
                indexed_properties,
                ..
            } => {
                if let Some(property) = indexed_properties.get(&index) {
                    return Ok(match property {
                        PropertyValue::Data { value, .. } => Some(*value),
                        PropertyValue::Accessor { .. } => None,
                    });
                }
                let Some(value) = elements.get(index).copied() else {
                    return Ok(None);
                };
                if value.is_hole() {
                    return Ok(None);
                }
                Ok(Some(value))
            }
            HeapValue::String { value, .. } => {
                // §22.1.3.2 — String indexing returns single UTF-16 code unit
                let code_unit = value.code_unit_at(index);
                match code_unit {
                    Some(unit) => {
                        let ch_str = JsString::from_utf16(vec![unit]);
                        let handle = self.alloc_js_string(ch_str);
                        Ok(Some(RegisterValue::from_object_handle(handle.0)))
                    }
                    None => Ok(None),
                }
            }
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::Closure { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Stores an indexed element on a dense array.
    pub fn set_index(
        &mut self,
        handle: ObjectHandle,
        index: usize,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        // Spec cap: a valid array index is `< 2^32 - 1` (ECMA-262 §7.1.22).
        // Rejecting at the lowest level keeps pathological tests like
        // `arr[2**32 - 1] = v` from growing a 32 GB `Vec<RegisterValue>`.
        if index >= MAX_ARRAY_LENGTH {
            return Err(ObjectError::InvalidArrayLength);
        }
        // Heap budget: reserve bytes for any sparse-hole grow before
        // borrowing the heap mutably. We compute the delta first, then
        // reserve, then perform the mutation. The mutation is scoped into
        // a helper that returns whether the reservation was actually used,
        // so we can release it on the "not extensible" exit path without
        // tripping the clippy `drop_non_drop` lint.
        let elem_size = std::mem::size_of::<RegisterValue>();
        let current_len = match self.object(handle)? {
            HeapValue::Array { elements, .. } => elements.len(),
            _ => return Err(ObjectError::InvalidKind),
        };
        let grow_delta = if index >= current_len {
            index
                .saturating_add(1)
                .saturating_sub(current_len)
                .saturating_mul(elem_size)
        } else {
            0
        };
        if grow_delta > 0 {
            self.heap.reserve_bytes(grow_delta)?;
        }
        let used_reservation = self.set_index_inner(handle, index, value)?;
        if grow_delta > 0 && !used_reservation {
            self.heap.release_bytes(grow_delta);
        }
        Ok(())
    }

    /// Inner helper for [`set_index`]. Returns `Ok(true)` when the caller's
    /// byte reservation was consumed by a real `Vec::resize`, `Ok(false)`
    /// when the array wasn't grown (so the reservation must be released).
    fn set_index_inner(
        &mut self,
        handle: ObjectHandle,
        index: usize,
        value: RegisterValue,
    ) -> Result<bool, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Array {
                extensible,
                elements,
                indexed_properties,
                elements_writable,
                length_writable,
                ..
            } => {
                let mut grew = false;
                if index >= elements.len() {
                    if !*extensible || !*length_writable {
                        return Ok(false);
                    }
                    elements.resize(index.saturating_add(1), RegisterValue::hole());
                    grew = true;
                }

                if let Some(property) = indexed_properties.get_mut(&index) {
                    match property {
                        PropertyValue::Data {
                            value: slot,
                            attributes,
                        } => {
                            if !attributes.writable() {
                                return Ok(grew);
                            }
                            *slot = value;
                            elements[index] = value;
                            return Ok(grew);
                        }
                        PropertyValue::Accessor { .. } => return Ok(grew),
                    }
                }

                if !*elements_writable {
                    return Ok(grew);
                }
                elements[index] = value;
                Ok(grew)
            }
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::String { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Appends a value to an array's element list.
    pub fn push_element(
        &mut self,
        handle: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Array {
                extensible,
                elements,
                elements_writable,
                length_writable,
                ..
            } => {
                if !*extensible || !*elements_writable || !*length_writable {
                    return Ok(());
                }
                elements.push(value);
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Resizes an array length while preserving sparse holes.
    pub fn set_array_length(
        &mut self,
        handle: ObjectHandle,
        length: usize,
    ) -> Result<bool, ObjectError> {
        // Spec cap: §22.1.3.1. `length > 2^32 - 1` is a `RangeError`.
        // Intercept here so we never attempt a `Vec::resize(2^32, ..)`.
        if length > MAX_ARRAY_LENGTH {
            return Err(ObjectError::InvalidArrayLength);
        }
        // Reserve the heap budget for the tracked slice so the shared OOM
        // flag is raised when the configured hard cap is crossed. Shrinks
        // and no-ops release an equivalent amount after the mutation — the
        // core mutation lives in `set_array_length_inner` so the match arm
        // can drop its mutable borrow before we call back into `self.heap`.
        let elem_size = std::mem::size_of::<RegisterValue>();
        let current_len = match self.object(handle)? {
            HeapValue::Array { elements, .. } => elements.len(),
            _ => return Err(ObjectError::InvalidKind),
        };
        if length > current_len {
            let additional = length.saturating_sub(current_len).saturating_mul(elem_size);
            self.heap.reserve_bytes(additional)?;
        }
        let outcome = self.set_array_length_inner(handle, length)?;
        // Reconcile the reservation with what actually happened inside the
        // mutation: release any bytes we optimistically reserved but did
        // not grow, and release any bytes freed by a shrink.
        let reserve_delta = length.saturating_sub(current_len).saturating_mul(elem_size);
        let release_delta = current_len.saturating_sub(length).saturating_mul(elem_size);
        if !outcome.grew {
            self.heap.release_bytes(reserve_delta);
        }
        if outcome.shrunk_bytes > 0 {
            self.heap.release_bytes(outcome.shrunk_bytes.min(release_delta));
        }
        Ok(outcome.ok)
    }

    /// Inner helper for [`set_array_length`]. Performs the mutation under a
    /// scoped mutable borrow, returning a small outcome struct so the
    /// caller can reconcile the heap budget after the borrow ends without
    /// tripping `clippy::drop_non_drop`.
    fn set_array_length_inner(
        &mut self,
        handle: ObjectHandle,
        length: usize,
    ) -> Result<SetLengthOutcome, ObjectError> {
        let elem_size = std::mem::size_of::<RegisterValue>();
        match self.object_mut(handle)? {
            HeapValue::Array {
                extensible,
                elements,
                indexed_properties,
                elements_configurable,
                length_writable,
                ..
            } => {
                if length < elements.len() {
                    if !*elements_configurable {
                        return Ok(SetLengthOutcome {
                            ok: false,
                            grew: false,
                            shrunk_bytes: 0,
                        });
                    }

                    let first_non_configurable =
                        indexed_properties
                            .range(length..)
                            .rev()
                            .find_map(|(&index, property)| {
                                (!property.attributes().configurable()).then_some(index)
                            });

                    if let Some(index) = first_non_configurable {
                        let removed = elements.len().saturating_sub(index.saturating_add(1));
                        elements.truncate(index.saturating_add(1));
                        indexed_properties.retain(|&key, _| key <= index);
                        return Ok(SetLengthOutcome {
                            ok: false,
                            grew: false,
                            shrunk_bytes: removed.saturating_mul(elem_size),
                        });
                    }

                    let removed = elements.len().saturating_sub(length);
                    elements.truncate(length);
                    indexed_properties.retain(|&key, _| key < length);
                    return Ok(SetLengthOutcome {
                        ok: true,
                        grew: false,
                        shrunk_bytes: removed.saturating_mul(elem_size),
                    });
                }
                if length > elements.len() {
                    if !*extensible || !*length_writable {
                        return Ok(SetLengthOutcome {
                            ok: false,
                            grew: false,
                            shrunk_bytes: 0,
                        });
                    }
                    elements.resize(length, RegisterValue::hole());
                    return Ok(SetLengthOutcome {
                        ok: true,
                        grew: true,
                        shrunk_bytes: 0,
                    });
                }
                Ok(SetLengthOutcome {
                    ok: true,
                    grew: false,
                    shrunk_bytes: 0,
                })
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the elements of an array as a Vec.
    pub fn array_elements(&self, handle: ObjectHandle) -> Result<Vec<RegisterValue>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Array { elements, .. } => Ok(elements.clone()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Allocates an internal iterator for a supported iterable.
    pub fn alloc_iterator(&mut self, iterable: ObjectHandle) -> Result<ObjectHandle, ObjectError> {
        // TypedArrays: first copy elements to a plain Array, then iterate that.
        if matches!(self.object(iterable)?, HeapValue::TypedArray { .. }) {
            let len = self.typed_array_length(iterable)?;
            let arr = self.alloc_array();
            for i in 0..len {
                let val = self.typed_array_get_element(iterable, i)?.unwrap_or(0.0);
                self.set_index(arr, i, RegisterValue::from_number(val))?;
            }
            let h = self.heap.alloc(HeapValue::ArrayIterator {
                prototype: None,
                iterable: arr,
                next_index: 0,
                closed: false,
                kind: ArrayIteratorKind::Values,
            });
            return Ok(ObjectHandle(h.0));
        }

        let iterator = match self.object(iterable)? {
            HeapValue::Array { .. } => HeapValue::ArrayIterator {
                prototype: None,
                iterable,
                next_index: 0,
                closed: false,
                kind: ArrayIteratorKind::Values,
            },
            HeapValue::String { .. } => HeapValue::StringIterator {
                prototype: None,
                iterable,
                next_index: 0,
                closed: false,
            },
            _ => return Err(ObjectError::InvalidKind),
        };

        let h = self.heap.alloc(iterator);
        Ok(ObjectHandle(h.0))
    }

    /// Allocates an Array iterator with explicit kind.
    /// Spec: <https://tc39.es/ecma262/#sec-createarrayiterator>
    pub fn alloc_array_iterator(
        &mut self,
        iterable: ObjectHandle,
        kind: ArrayIteratorKind,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::ArrayIterator {
            prototype: None,
            iterable,
            next_index: 0,
            closed: false,
            kind,
        });
        ObjectHandle(h.0)
    }

    /// Allocates a String iterator.
    /// Spec: <https://tc39.es/ecma262/#sec-createstringiterator>
    pub fn alloc_string_iterator(&mut self, iterable: ObjectHandle) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::StringIterator {
            prototype: None,
            iterable,
            next_index: 0,
            closed: false,
        });
        ObjectHandle(h.0)
    }

    /// Allocates an internal Map iterator.
    pub fn alloc_map_iterator(
        &mut self,
        iterable: ObjectHandle,
        kind: MapIteratorKind,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::MapIterator {
            prototype: None,
            iterable,
            next_index: 0,
            closed: false,
            kind,
        });
        ObjectHandle(h.0)
    }

    /// Allocates an internal Set iterator.
    pub fn alloc_set_iterator(
        &mut self,
        iterable: ObjectHandle,
        kind: SetIteratorKind,
    ) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::SetIterator {
            prototype: None,
            iterable,
            next_index: 0,
            closed: false,
            kind,
        });
        ObjectHandle(h.0)
    }

    /// Advances an internal iterator by one step.
    pub fn iterator_next(&mut self, handle: ObjectHandle) -> Result<IteratorStep, ObjectError> {
        enum IteratorKind {
            Array,
            String,
        }

        let (iterable, next_index, closed, kind) = match self.object(handle)? {
            // Fast path only for values-kind array iterators.
            HeapValue::ArrayIterator {
                iterable,
                next_index,
                closed,
                kind: ArrayIteratorKind::Values,
                ..
            } => (*iterable, *next_index, *closed, IteratorKind::Array),
            HeapValue::StringIterator {
                iterable,
                next_index,
                closed,
                ..
            } => (*iterable, *next_index, *closed, IteratorKind::String),
            // Keys/entries array iterators and Map/Set iterators use protocol .next().
            HeapValue::ArrayIterator { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. } => {
                return Err(ObjectError::InvalidKind);
            }
            _ => return Err(ObjectError::InvalidKind),
        };

        if closed {
            return Ok(IteratorStep::done());
        }

        let mut string_extra_advance: usize = 0;
        let step = match kind {
            IteratorKind::Array => match self.array_length(iterable)? {
                Some(length) if next_index < length => {
                    match self.get_index(iterable, next_index)? {
                        Some(value) => IteratorStep::yield_value(value),
                        None => IteratorStep::yield_value(RegisterValue::undefined()),
                    }
                }
                _ => IteratorStep::done(),
            },
            IteratorKind::String => {
                // §22.1.5.2.1 %StringIteratorPrototype%.next() — yield code points,
                // not individual code units. Surrogate pairs yield a single 2-unit string.
                // Spec: <https://tc39.es/ecma262/#sec-%stringiteratorprototype%.next>
                let Some(js_str) = self.string_value(iterable)?.cloned() else {
                    return Ok(IteratorStep::done());
                };
                let utf16 = js_str.as_utf16();
                if next_index >= utf16.len() {
                    IteratorStep::done()
                } else {
                    let (_, advance) = js_str
                        .code_point_at(next_index)
                        .unwrap_or((utf16[next_index] as u32, 1));
                    let ch_units = utf16[next_index..next_index + advance].to_vec();
                    let ch_str = JsString::from_utf16(ch_units);
                    let str_handle = self.alloc_js_string(ch_str);
                    // Store extra advance for the post-step block (advance-1 extra beyond +1)
                    string_extra_advance = advance.saturating_sub(1);
                    IteratorStep::yield_value(RegisterValue::from_object_handle(str_handle.0))
                }
            }
        };

        match self.object_mut(handle)? {
            HeapValue::ArrayIterator {
                next_index, closed, ..
            }
            | HeapValue::StringIterator {
                next_index, closed, ..
            } => {
                if step.is_done() {
                    *closed = true;
                } else {
                    *next_index = next_index.wrapping_add(1 + string_extra_advance);
                }
            }
            _ => return Err(ObjectError::InvalidKind),
        }

        Ok(step)
    }

    pub fn iterator_cursor(&self, handle: ObjectHandle) -> Result<IteratorCursor, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayIterator {
                iterable,
                next_index,
                closed,
                ..
            } => Ok(IteratorCursor {
                iterable: *iterable,
                next_index: *next_index,
                closed: *closed,
                is_array: true,
            }),
            HeapValue::StringIterator {
                iterable,
                next_index,
                closed,
                ..
            } => Ok(IteratorCursor {
                iterable: *iterable,
                next_index: *next_index,
                closed: *closed,
                is_array: false,
            }),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    pub fn advance_iterator_cursor(
        &mut self,
        handle: ObjectHandle,
        done: bool,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::ArrayIterator {
                next_index, closed, ..
            }
            | HeapValue::StringIterator {
                next_index, closed, ..
            } => {
                if done {
                    *closed = true;
                } else {
                    *next_index = next_index.saturating_add(1);
                }
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Closes an internal iterator.
    pub fn iterator_close(&mut self, handle: ObjectHandle) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::ArrayIterator { closed, .. }
            | HeapValue::StringIterator { closed, .. }
            | HeapValue::MapIterator { closed, .. }
            | HeapValue::SetIterator { closed, .. } => {
                *closed = true;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the `ArrayIteratorKind` for an Array iterator.
    /// Spec: <https://tc39.es/ecma262/#sec-%arrayiteratorprototype%.next>
    pub fn array_iterator_kind(
        &self,
        handle: ObjectHandle,
    ) -> Result<ArrayIteratorKind, ObjectError> {
        match self.object(handle)? {
            HeapValue::ArrayIterator { kind, .. } => Ok(*kind),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns `(iterable, next_index, closed, kind)` for a Map iterator.
    /// Spec: <https://tc39.es/ecma262/#sec-%mapiteratorprototype%.next>
    pub fn map_iterator_state(
        &self,
        handle: ObjectHandle,
    ) -> Result<(ObjectHandle, usize, bool, MapIteratorKind), ObjectError> {
        match self.object(handle)? {
            HeapValue::MapIterator {
                iterable,
                next_index,
                closed,
                kind,
                ..
            } => Ok((*iterable, *next_index, *closed, *kind)),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Advances a Map iterator's cursor to `index`.
    pub fn set_map_iterator_index(
        &mut self,
        handle: ObjectHandle,
        index: usize,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::MapIterator { next_index, .. } => {
                *next_index = index;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns `(iterable, next_index, closed, kind)` for a Set iterator.
    /// Spec: <https://tc39.es/ecma262/#sec-%setiteratorprototype%.next>
    pub fn set_iterator_state(
        &self,
        handle: ObjectHandle,
    ) -> Result<(ObjectHandle, usize, bool, SetIteratorKind), ObjectError> {
        match self.object(handle)? {
            HeapValue::SetIterator {
                iterable,
                next_index,
                closed,
                kind,
                ..
            } => Ok((*iterable, *next_index, *closed, *kind)),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Advances a Set iterator's cursor to `index`.
    pub fn set_set_iterator_index(
        &mut self,
        handle: ObjectHandle,
        index: usize,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::SetIterator { next_index, .. } => {
                *next_index = index;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns raw Set entries (including deleted `None` slots) for lazy iteration.
    /// Spec: <https://tc39.es/ecma262/#sec-set-objects>
    pub fn set_entries(
        &self,
        handle: ObjectHandle,
    ) -> Result<Vec<Option<RegisterValue>>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => Ok(entries.clone()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns raw Map entries (including deleted `None` slots) for lazy iteration.
    /// Spec: <https://tc39.es/ecma262/#sec-map-objects>
    pub fn map_entries_raw(
        &self,
        handle: ObjectHandle,
    ) -> Result<Vec<Option<(RegisterValue, RegisterValue)>>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => Ok(entries.clone()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Creates a property key iterator for `for..in` enumeration.
    /// Collects all enumerable string property keys from the object and its prototype chain,
    /// and pre-allocates string handles for them.
    /// Creates an empty property iterator (for null/undefined/primitives in for..in).
    pub fn alloc_empty_property_iterator(&mut self) -> Result<ObjectHandle, ObjectError> {
        let h = self.heap.alloc(HeapValue::PropertyIterator {
            key_handles: Vec::new(),
            next_index: 0,
        });
        Ok(ObjectHandle(h.0))
    }

    /// ES2024 §14.7.5.6 EnumerateObjectProperties — collects enumerable string
    /// property keys from the object and its prototype chain for `for..in`.
    ///
    /// Spec algorithm:
    ///   visited = new Set()
    ///   for each O in (receiver, proto, proto's proto, ...):
    ///     for each key in O.[[OwnPropertyKeys]]():
    ///       if key is Symbol: continue
    ///       if visited has key: continue
    ///       visited.add(key)                      ← always, even if non-enumerable
    ///       if O.[[GetOwnProperty]](key).[[Enumerable]]: yield key
    ///
    /// **Shadowing**: a non-enumerable property in a descendant still blocks
    /// same-name enumerable properties in ancestors. Therefore `visited.add`
    /// precedes the enumerable check.
    pub fn alloc_property_iterator(
        &mut self,
        object: ObjectHandle,
        property_names: &mut PropertyNameRegistry,
    ) -> Result<ObjectHandle, ObjectError> {
        let mut name_ids: Vec<PropertyNameId> = Vec::new();
        let mut seen: std::collections::HashSet<PropertyNameId> = std::collections::HashSet::new();
        let mut current = Some(object);
        while let Some(h) = current {
            // Use `own_keys_with_registry` which interns Array indexed elements,
            // Array `length`, and String `length` as proper PropertyNameIds.
            let obj_keys = match self.own_keys_with_registry(h, property_names) {
                Ok(keys) => keys,
                Err(ObjectError::InvalidKind) => Vec::new(),
                Err(e) => return Err(e),
            };
            let proto = self.get_prototype(h)?;

            for key in obj_keys {
                // Symbols are never enumerable via for-in (ES §13.7.5.15).
                if property_names.is_symbol(key) {
                    continue;
                }
                // Shadowing: always mark visited, even if non-enumerable —
                // this blocks same-name properties in ancestors.
                if !seen.insert(key) {
                    continue;
                }
                // Check [[Enumerable]] for yielding.
                let enumerable = match self.own_property_descriptor(h, key, property_names)? {
                    Some(prop) => prop.attributes().enumerable(),
                    None => continue,
                };
                if enumerable {
                    name_ids.push(key);
                }
            }
            current = proto;
        }

        // Pre-allocate string handles for all collected keys.
        let key_handles: Vec<ObjectHandle> = name_ids
            .iter()
            .filter_map(|id| property_names.get(*id).map(|name| self.alloc_string(name)))
            .collect();

        let h = self.heap.alloc(HeapValue::PropertyIterator {
            key_handles,
            next_index: 0,
        });
        Ok(ObjectHandle(h.0))
    }

    /// Advances a property key iterator by one step.
    pub fn property_iterator_next(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<IteratorStep, ObjectError> {
        let (next, key_handle) = match self.object(handle)? {
            HeapValue::PropertyIterator {
                key_handles,
                next_index,
            } => {
                if *next_index >= key_handles.len() {
                    return Ok(IteratorStep::done());
                }
                (*next_index, key_handles[*next_index])
            }
            _ => return Err(ObjectError::InvalidKind),
        };
        match self.object_mut(handle)? {
            HeapValue::PropertyIterator { next_index, .. } => {
                *next_index = next + 1;
            }
            _ => unreachable!(),
        }
        Ok(IteratorStep::yield_value(
            RegisterValue::from_object_handle(key_handle.0),
        ))
    }

    /// Returns the callee stored in a closure object.
    pub fn closure_callee(&self, handle: ObjectHandle) -> Result<FunctionIndex, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { callee, .. } => Ok(*callee),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the `[[Realm]]` slot stored on a callable object, if any.
    /// Returns `None` for non-callable values; for bound functions and proxies the
    /// caller is responsible for traversing further per §10.2.3 GetFunctionRealm.
    /// Spec: <https://tc39.es/ecma262/#sec-getfunctionrealm>
    pub fn function_realm(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<crate::realm::RealmId>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { realm, .. }
            | HeapValue::HostFunction { realm, .. }
            | HeapValue::BoundFunction { realm, .. } => Ok(Some(*realm)),
            _ => Ok(None),
        }
    }

    /// Returns the target stored in a bound function exotic object.
    pub fn bound_function_target(&self, handle: ObjectHandle) -> Result<ObjectHandle, ObjectError> {
        match self.object(handle)? {
            HeapValue::BoundFunction { target, .. } => Ok(*target),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the owning module stored in a closure object.
    pub fn closure_module(&self, handle: ObjectHandle) -> Result<Module, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { module, .. } => Ok(module.clone()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §15.7.14 — Returns the field initializer closure stored on a class constructor, if any.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    pub fn closure_field_initializer(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure {
                field_initializer, ..
            } => Ok(*field_initializer),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §15.7.14 — Stores a field initializer closure on a class constructor.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    pub fn set_closure_field_initializer(
        &mut self,
        handle: ObjectHandle,
        initializer: ObjectHandle,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Closure {
                field_initializer, ..
            } => {
                *field_initializer = Some(initializer);
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  §6.2.12 Private Names — class_id, private_methods, private_elements
    // ═══════════════════════════════════════════════════════════════════════

    /// §6.2.12 — Returns the class_id stored on a closure (0 if none).
    /// Spec: <https://tc39.es/ecma262/#sec-private-names>
    pub fn closure_class_id(&self, handle: ObjectHandle) -> Result<u64, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { class_id, .. } => Ok(*class_id),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §6.2.12 — Sets the class_id on a closure.
    /// Spec: <https://tc39.es/ecma262/#sec-private-names>
    pub fn set_closure_class_id(
        &mut self,
        handle: ObjectHandle,
        id: u64,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Closure { class_id, .. } => {
                *class_id = id;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §15.7.14 — Pushes a private method/accessor to constructor's `[[PrivateMethods]]`.
    /// These are copied to each instance during InitializeInstanceElements.
    /// Spec: <https://tc39.es/ecma262/#sec-initializeinstanceelements>
    pub fn push_private_method(
        &mut self,
        handle: ObjectHandle,
        key: PrivateNameKey,
        element: PrivateElement,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Closure {
                private_methods, ..
            } => {
                // Merge accessor pairs: if an accessor with the same key exists,
                // merge the getter/setter instead of pushing a duplicate.
                if let PrivateElement::Accessor { getter, setter } = &element {
                    for (k, existing) in private_methods.iter_mut() {
                        if *k == key
                            && let PrivateElement::Accessor {
                                getter: g,
                                setter: s,
                            } = existing
                        {
                            if getter.is_some() {
                                *g = *getter;
                            }
                            if setter.is_some() {
                                *s = *setter;
                            }
                            return Ok(());
                        }
                    }
                }
                private_methods.push((key, element));
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §15.7.14 — Returns a clone of the constructor's `[[PrivateMethods]]` list.
    /// Spec: <https://tc39.es/ecma262/#sec-initializeinstanceelements>
    pub fn closure_private_methods(
        &self,
        handle: ObjectHandle,
    ) -> Result<Vec<(PrivateNameKey, PrivateElement)>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure {
                private_methods, ..
            } => Ok(private_methods.clone()),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// §7.3.31 PrivateFieldAdd — append a private field to `[[PrivateElements]]`.
    /// Throws TypeError if a field with the same key already exists.
    /// Spec: <https://tc39.es/ecma262/#sec-privatefieldadd>
    pub fn private_field_add(
        &mut self,
        handle: ObjectHandle,
        key: PrivateNameKey,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        let elements = self.private_elements_mut(handle)?;
        if elements.iter().any(|(k, _)| *k == key) {
            return Err(ObjectError::TypeError(
                "private field already defined on object".into(),
            ));
        }
        elements.push((key, PrivateElement::Field(value)));
        Ok(())
    }

    /// §7.3.31 PrivateMethodOrAccessorAdd — append a method/accessor to `[[PrivateElements]]`.
    /// Spec: <https://tc39.es/ecma262/#sec-privatemethodoraccessoradd>
    pub fn private_method_or_accessor_add(
        &mut self,
        handle: ObjectHandle,
        key: PrivateNameKey,
        element: PrivateElement,
    ) -> Result<(), ObjectError> {
        let elements = self.private_elements_mut(handle)?;
        // Merge accessor pairs.
        if let PrivateElement::Accessor { getter, setter } = &element {
            for (k, existing) in elements.iter_mut() {
                if *k == key
                    && let PrivateElement::Accessor {
                        getter: g,
                        setter: s,
                    } = existing
                {
                    if getter.is_some() {
                        *g = *getter;
                    }
                    if setter.is_some() {
                        *s = *setter;
                    }
                    return Ok(());
                }
            }
        }
        if elements.iter().any(|(k, _)| *k == key) {
            return Err(ObjectError::TypeError(
                "private method/accessor already defined on object".into(),
            ));
        }
        elements.push((key, element));
        Ok(())
    }

    /// §7.3.32 PrivateGet — read a private field, method, or accessor.
    /// Spec: <https://tc39.es/ecma262/#sec-privateget>
    pub fn private_get(
        &self,
        handle: ObjectHandle,
        key: &PrivateNameKey,
    ) -> Result<RegisterValue, ObjectError> {
        let elements = self.private_elements(handle)?;
        for (k, element) in elements {
            if k == key {
                return match element {
                    PrivateElement::Field(value) => Ok(*value),
                    PrivateElement::Method(method) => {
                        Ok(RegisterValue::from_object_handle(method.0))
                    }
                    PrivateElement::Accessor { getter, .. } => {
                        if let Some(g) = getter {
                            // Accessor getter must be called by the interpreter,
                            // so we return the getter function handle.
                            Ok(RegisterValue::from_object_handle(g.0))
                        } else {
                            Err(ObjectError::TypeError(
                                "private accessor has no getter".into(),
                            ))
                        }
                    }
                };
            }
        }
        Err(ObjectError::TypeError(
            "cannot access private field or method: object does not have the private member".into(),
        ))
    }

    /// §7.3.33 PrivateSet — write a private field value.
    /// Methods throw TypeError (read-only). Accessors invoke the setter.
    /// Spec: <https://tc39.es/ecma262/#sec-privateset>
    pub fn private_set(
        &mut self,
        handle: ObjectHandle,
        key: &PrivateNameKey,
        value: RegisterValue,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        let elements = self.private_elements_mut(handle)?;
        for (k, element) in elements.iter_mut() {
            if k == key {
                return match element {
                    PrivateElement::Field(field_value) => {
                        *field_value = value;
                        Ok(None)
                    }
                    PrivateElement::Method(_) => Err(ObjectError::TypeError(
                        "cannot assign to a private method".into(),
                    )),
                    PrivateElement::Accessor { setter, .. } => {
                        if let Some(s) = setter {
                            // Accessor setter must be called by the interpreter.
                            Ok(Some(*s))
                        } else {
                            Err(ObjectError::TypeError(
                                "private accessor has no setter".into(),
                            ))
                        }
                    }
                };
            }
        }
        Err(ObjectError::TypeError(
            "cannot set private field: object does not have the private member".into(),
        ))
    }

    /// §7.3.31 PrivateElementFind — check if a private element exists.
    /// Spec: <https://tc39.es/ecma262/#sec-privateelementfind>
    pub fn private_element_find(
        &self,
        handle: ObjectHandle,
        key: &PrivateNameKey,
    ) -> Result<bool, ObjectError> {
        let elements = self.private_elements(handle)?;
        Ok(elements.iter().any(|(k, _)| k == key))
    }

    /// Returns a reference to the `[[PrivateElements]]` list of an object.
    fn private_elements(
        &self,
        handle: ObjectHandle,
    ) -> Result<&[(PrivateNameKey, PrivateElement)], ObjectError> {
        match self.object(handle)? {
            HeapValue::Object {
                private_elements, ..
            }
            | HeapValue::Closure {
                private_elements, ..
            } => Ok(private_elements),
            _ => Ok(&[]),
        }
    }

    /// Returns a mutable reference to the `[[PrivateElements]]` list.
    fn private_elements_mut(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<&mut Vec<(PrivateNameKey, PrivateElement)>, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Object {
                private_elements, ..
            }
            | HeapValue::Closure {
                private_elements, ..
            } => Ok(private_elements),
            _ => Err(ObjectError::TypeError(
                "object does not support private elements".into(),
            )),
        }
    }

    /// Returns a reference to a specific private element by key, if found.
    pub fn private_elements_ref(
        &self,
        handle: ObjectHandle,
        key: &PrivateNameKey,
    ) -> Option<&PrivateElement> {
        let elements = self.private_elements(handle).ok()?;
        elements.iter().find(|(k, _)| k == key).map(|(_, e)| e)
    }

    /// Returns the host-function id stored in a host-function object, if any.
    pub fn host_function(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<HostFunctionId>, ObjectError> {
        match self.object(handle)? {
            HeapValue::HostFunction { function, .. } => Ok(Some(*function)),
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::Array { .. }
            | HeapValue::String { .. }
            | HeapValue::Closure { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => Ok(None),
        }
    }

    /// Returns the dense element length for arrays.
    pub fn array_length(&self, handle: ObjectHandle) -> Result<Option<usize>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Array { elements, .. } => Ok(Some(elements.len())),
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::String { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => Ok(None),
        }
    }

    /// Returns the WTF-16 string contents for string heap values.
    ///
    /// Returns a reference to the internal `JsString` which stores UTF-16
    /// code units (including lone surrogates).
    pub fn string_value(&self, handle: ObjectHandle) -> Result<Option<&JsString>, ObjectError> {
        match self.object(handle)? {
            HeapValue::String { value, .. } => Ok(Some(value)),
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::Array { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => Ok(None),
        }
    }

    /// Returns the captured upvalue handle for a closure slot.
    pub fn closure_upvalue(
        &self,
        handle: ObjectHandle,
        index: usize,
    ) -> Result<ObjectHandle, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { upvalues, .. } => upvalues
                .get(index)
                .copied()
                .ok_or(ObjectError::InvalidIndex),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the payload link stored on a native object, if any.
    pub fn native_payload_id(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<NativePayloadId>, ObjectError> {
        match self.object(handle)? {
            HeapValue::NativeObject { payload, .. } => Ok(Some(*payload)),
            HeapValue::Object { .. }
            | HeapValue::Array { .. }
            | HeapValue::String { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => Ok(None),
        }
    }

    /// Visits every live native payload link stored in the heap.
    pub fn trace_native_payload_links(
        &self,
        tracer: &mut dyn FnMut(ObjectHandle, NativePayloadId),
    ) {
        self.heap.for_each(|index, any| {
            if let Some(HeapValue::NativeObject { payload, .. }) = any.downcast_ref::<HeapValue>() {
                tracer(ObjectHandle(index), *payload);
            }
        });
    }

    /// Reads a value from an upvalue cell.
    pub fn get_upvalue(&self, handle: ObjectHandle) -> Result<RegisterValue, ObjectError> {
        match self.object(handle)? {
            HeapValue::UpvalueCell { value } => Ok(*value),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Writes a value into an upvalue cell.
    pub fn set_upvalue(
        &mut self,
        handle: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::UpvalueCell { value: slot } => {
                *slot = value;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns a monomorphic cache hit when the cached shape and slot still match.
    pub fn get_cached(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
        cache: PropertyInlineCache,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        let object = self.object(handle)?;
        let (shape_id, keys, values) = match object {
            HeapValue::Object {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::BoundFunction {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::SharedArrayBuffer {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::DataView {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id,
                keys,
                values,
                ..
            } => (shape_id, keys, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        if *shape_id != cache.shape_id() {
            return Ok(None);
        }

        let slot_index = usize::from(cache.slot_index());
        if keys.get(slot_index) == Some(&property) {
            return Ok(values.get(slot_index).copied());
        }

        Ok(None)
    }

    /// Returns a property value through ordinary prototype traversal.
    pub fn get_property(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyLookup>, ObjectError> {
        self.get_property_with_registry(handle, property, &PropertyNameRegistry::default())
    }

    /// Returns a named property lookup using the caller's property-name registry.
    pub fn get_property_with_registry(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
        property_names: &PropertyNameRegistry,
    ) -> Result<Option<PropertyLookup>, ObjectError> {
        let mut current = Some(handle);
        let mut depth = 0;
        while let Some(owner) = current {
            if let Some((value, cache)) = self.get_own_property(owner, property, property_names)? {
                let cache = (owner == handle).then_some(cache);
                return Ok(Some(PropertyLookup::new(owner, value, cache)));
            }
            depth += 1;
            if depth > MAX_PROTOTYPE_DEPTH {
                break;
            }
            current = self.property_traversal_prototype(owner)?;
        }
        Ok(None)
    }

    /// Returns a shaped property value when the shape and slot still match.
    pub fn get_shaped(
        &self,
        handle: ObjectHandle,
        shape_id: ObjectShapeId,
        slot_index: u16,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        let object = self.object(handle)?;
        let (object_shape_id, values) = match object {
            HeapValue::Object {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::SharedArrayBuffer {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::DataView {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id: object_shape_id,
                values,
                ..
            } => (object_shape_id, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        if *object_shape_id != shape_id {
            return Ok(None);
        }
        Ok(values.get(usize::from(slot_index)).copied())
    }

    /// Writes a property through the monomorphic cache when it still matches.
    pub fn set_cached(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
        cache: PropertyInlineCache,
    ) -> Result<bool, ObjectError> {
        let object = self.object_mut(handle)?;
        let (shape_id, keys, values) = match object {
            HeapValue::Object {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::BoundFunction {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id,
                keys,
                values,
                ..
            } => (shape_id, keys, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        if *shape_id != cache.shape_id() {
            return Ok(false);
        }

        let slot_index = usize::from(cache.slot_index());
        if keys.get(slot_index) == Some(&property)
            && let Some(slot) = values.get_mut(slot_index)
        {
            match slot {
                PropertyValue::Data {
                    value: slot_value,
                    attributes,
                } => {
                    if !attributes.writable() {
                        return Ok(false);
                    }
                    *slot_value = value;
                    return Ok(true);
                }
                PropertyValue::Accessor { .. } => return Ok(false),
            }
        }

        Ok(false)
    }

    /// Writes a property through the generic path and returns an updated cache.
    pub fn set_property(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<PropertyInlineCache, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. } => self.set_named_property_storage(handle, property, value),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Writes a property using the caller's property registry.
    pub fn set_property_with_registry(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
        property_names: &PropertyNameRegistry,
    ) -> Result<PropertyInlineCache, ObjectError> {
        match self.object(handle)? {
            HeapValue::Array { .. } => {
                let Some(property_name) = property_names.get(property) else {
                    return Err(ObjectError::InvalidKind);
                };
                if property_name == "length" {
                    self.set_array_length_from_value(handle, value)?;
                    return Ok(PropertyInlineCache::new(ObjectShapeId(0), 0));
                }
                if let Some(index) = canonical_array_index(property_name) {
                    self.set_index(handle, index, value)?;
                    return Ok(PropertyInlineCache::new(ObjectShapeId(0), 0));
                }
                self.set_array_named_property_storage(handle, property, value)
            }
            _ => self.set_property(handle, property, value),
        }
    }

    /// Deletes an own named property from one ordinary object-like heap value.
    pub fn delete_property(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. } => self.delete_ordinary_property(handle, property),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Deletes an own property using the caller's property registry.
    pub fn delete_property_with_registry(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        property_names: &PropertyNameRegistry,
    ) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. } => self.delete_ordinary_property(handle, property),
            HeapValue::Array { .. } => self.delete_array_property(handle, property, property_names),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    fn delete_ordinary_property(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, ObjectError> {
        self.delete_named_property_storage(handle, property, false)
    }

    fn delete_array_property(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        property_names: &PropertyNameRegistry,
    ) -> Result<bool, ObjectError> {
        let Some(property_name) = property_names.get(property) else {
            return Ok(true);
        };
        if property_name == "length" {
            return Ok(false);
        }
        if let Some(index) = canonical_array_index(property_name) {
            return match self.object_mut(handle)? {
                HeapValue::Array {
                    elements,
                    indexed_properties,
                    elements_configurable,
                    ..
                } => {
                    if let Some(property) = indexed_properties.get(&index) {
                        if !property.attributes().configurable() {
                            return Ok(false);
                        }
                        indexed_properties.remove(&index);
                        if let Some(value) = elements.get_mut(index) {
                            *value = RegisterValue::hole();
                        }
                        return Ok(true);
                    }
                    let Some(value) = elements.get_mut(index) else {
                        return Ok(true);
                    };
                    if value.is_hole() {
                        return Ok(true);
                    }
                    if !*elements_configurable {
                        return Ok(false);
                    }
                    *value = RegisterValue::hole();
                    Ok(true)
                }
                _ => Err(ObjectError::InvalidKind),
            };
        }

        self.delete_named_property_storage(handle, property, true)
    }

    /// ES2024 §10.1.6 `[[DefineOwnProperty]]` — defines or replaces a property
    /// with full attribute control.
    pub fn define_own_property(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        desc: PropertyValue,
    ) -> Result<bool, ObjectError> {
        self.define_own_property_from_descriptor(
            handle,
            property,
            PropertyDescriptor::from_property_value(desc),
        )
    }

    /// ES2024 §10.1.6 `[[DefineOwnProperty]]` using a partial property descriptor.
    pub fn define_own_property_from_descriptor(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        desc: PropertyDescriptor,
    ) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. } => {
                self.define_ordinary_own_property_from_descriptor(handle, property, desc)
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// ES2024 §10.1.6 `[[DefineOwnProperty]]` using the caller's property registry.
    pub fn define_own_property_from_descriptor_with_registry(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        desc: PropertyDescriptor,
        property_names: &PropertyNameRegistry,
    ) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. }
            | HeapValue::NativeObject { .. }
            | HeapValue::Closure { .. }
            | HeapValue::HostFunction { .. }
            | HeapValue::BoundFunction { .. }
            | HeapValue::ArrayBuffer { .. }
            | HeapValue::SharedArrayBuffer { .. }
            | HeapValue::DataView { .. }
            | HeapValue::TypedArray { .. }
            | HeapValue::RegExp { .. } => {
                self.define_ordinary_own_property_from_descriptor(handle, property, desc)
            }
            HeapValue::Array { .. } => self.define_array_own_property_from_descriptor(
                handle,
                property,
                desc,
                property_names,
            ),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    fn define_ordinary_own_property_from_descriptor(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        desc: PropertyDescriptor,
    ) -> Result<bool, ObjectError> {
        self.define_named_property_storage(handle, property, desc, false)
    }

    fn define_array_own_property_from_descriptor(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        desc: PropertyDescriptor,
        property_names: &PropertyNameRegistry,
    ) -> Result<bool, ObjectError> {
        let Some(property_name) = property_names.get(property) else {
            return Ok(false);
        };

        if property_name == "length" {
            return self.define_array_length_property_from_descriptor(handle, desc);
        }

        if let Some(index) = canonical_array_index(property_name) {
            return self.define_array_index_property_from_descriptor(handle, index, desc);
        }

        self.define_array_named_property_from_descriptor(handle, property, desc)
    }

    fn define_array_length_property_from_descriptor(
        &mut self,
        handle: ObjectHandle,
        desc: PropertyDescriptor,
    ) -> Result<bool, ObjectError> {
        if matches!(desc.kind(), PropertyDescriptorKind::Accessor { .. }) {
            return Ok(false);
        }

        let (current_length, current_writable) = match self.object(handle)? {
            HeapValue::Array {
                elements,
                length_writable,
                ..
            } => (elements.len(), *length_writable),
            _ => return Err(ObjectError::InvalidKind),
        };

        let current = PropertyValue::data_with_attrs(
            RegisterValue::from_i32(i32::try_from(current_length).unwrap_or(i32::MAX)),
            PropertyAttributes::from_flags(current_writable, false, false),
        );
        let Some(next) = self.apply_property_descriptor(current, desc)? else {
            return Ok(false);
        };

        let PropertyValue::Data { value, attributes } = next else {
            return Ok(false);
        };
        if attributes.enumerable() || attributes.configurable() {
            return Ok(false);
        }

        let Some(next_length) = array_length_from_value(value) else {
            return Ok(false);
        };

        if !self.set_array_length(handle, next_length)? {
            return Ok(false);
        }

        let HeapValue::Array {
            length_writable, ..
        } = self.object_mut(handle)?
        else {
            return Err(ObjectError::InvalidKind);
        };
        *length_writable = attributes.writable();
        Ok(true)
    }

    fn define_array_index_property_from_descriptor(
        &mut self,
        handle: ObjectHandle,
        index: usize,
        desc: PropertyDescriptor,
    ) -> Result<bool, ObjectError> {
        // Spec cap (ECMA-262 §7.1.22): array indices must fit in uint32.
        // This intercepts patterns like `Object.defineProperty(arr, 2**32-1, ..)`
        // before they grow `elements` past 32 GB.
        if index >= MAX_ARRAY_LENGTH {
            return Err(ObjectError::InvalidArrayLength);
        }
        let existing = match self.object(handle)? {
            HeapValue::Array {
                elements,
                indexed_properties,
                elements_writable,
                elements_configurable,
                ..
            } => indexed_properties.get(&index).copied().or_else(|| {
                elements
                    .get(index)
                    .copied()
                    .filter(|value| !value.is_hole())
                    .map(|value| {
                        PropertyValue::data_with_attrs(
                            value,
                            PropertyAttributes::from_flags(
                                *elements_writable,
                                true,
                                *elements_configurable,
                            ),
                        )
                    })
            }),
            _ => return Err(ObjectError::InvalidKind),
        };

        let next = if let Some(existing) = existing {
            let Some(next) = self.apply_property_descriptor(existing, desc)? else {
                return Ok(false);
            };
            next
        } else {
            if !self.is_extensible(handle)? {
                return Ok(false);
            }
            self.new_property_from_descriptor(desc)
        };

        // Reserve the storage for the dense-vector growth against the
        // configured heap cap BEFORE calling `elements.resize`. Without this,
        // a pattern like `Object.defineProperty(arr, 4294967294, { ... })`
        // would force a 32 GB `Vec<Value>::resize` call on the Rust side —
        // bypassing the Otter heap cap and OOM-ing the host process.
        // `set_array_length` / `set_index` use the same reservation pattern;
        // this brings `[[DefineOwnProperty]]` on array indices into line.
        let elem_size = std::mem::size_of::<RegisterValue>();
        let current_len = match self.object(handle)? {
            HeapValue::Array { elements, .. } => elements.len(),
            _ => return Err(ObjectError::InvalidKind),
        };
        let grow_delta = if index >= current_len {
            index
                .saturating_add(1)
                .saturating_sub(current_len)
                .saturating_mul(elem_size)
        } else {
            0
        };
        if grow_delta > 0 {
            self.heap.reserve_bytes(grow_delta)?;
        }

        let array = self.object_mut(handle)?;
        let HeapValue::Array {
            extensible,
            elements,
            indexed_properties,
            elements_writable,
            elements_configurable,
            length_writable,
            ..
        } = array
        else {
            return Err(ObjectError::InvalidKind);
        };

        let consumed_reservation = match next {
            PropertyValue::Data { value, attributes } => {
                let mut consumed = false;
                if index >= elements.len() {
                    if !*extensible || !*length_writable {
                        return {
                            if grow_delta > 0 {
                                self.heap.release_bytes(grow_delta);
                            }
                            Ok(false)
                        };
                    }
                    elements.resize(index.saturating_add(1), RegisterValue::hole());
                    consumed = true;
                }
                elements[index] = value;

                let default =
                    array_index_default_attributes(*elements_writable, *elements_configurable);
                if attributes == default {
                    indexed_properties.remove(&index);
                } else {
                    indexed_properties
                        .insert(index, PropertyValue::data_with_attrs(value, attributes));
                }
                consumed
            }
            PropertyValue::Accessor {
                getter,
                setter,
                attributes,
            } => {
                let mut consumed = false;
                if index >= elements.len() {
                    if !*extensible || !*length_writable {
                        return {
                            if grow_delta > 0 {
                                self.heap.release_bytes(grow_delta);
                            }
                            Ok(false)
                        };
                    }
                    elements.resize(index.saturating_add(1), RegisterValue::hole());
                    consumed = true;
                } else {
                    elements[index] = RegisterValue::hole();
                }
                indexed_properties.insert(
                    index,
                    PropertyValue::Accessor {
                        getter,
                        setter,
                        attributes,
                    },
                );
                let _ = (elements_writable, elements_configurable);
                consumed
            }
        };

        if grow_delta > 0 && !consumed_reservation {
            self.heap.release_bytes(grow_delta);
        }

        Ok(true)
    }

    fn define_array_named_property_from_descriptor(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        desc: PropertyDescriptor,
    ) -> Result<bool, ObjectError> {
        self.define_named_property_storage(handle, property, desc, true)
    }

    /// Writes a shaped property value when the shape and slot still match.
    pub fn set_shaped(
        &mut self,
        handle: ObjectHandle,
        shape_id: ObjectShapeId,
        slot_index: u16,
        value: RegisterValue,
    ) -> Result<bool, ObjectError> {
        let object = self.object_mut(handle)?;
        let (object_shape_id, values) = match object {
            HeapValue::Object {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::SharedArrayBuffer {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::DataView {
                shape_id: object_shape_id,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id: object_shape_id,
                values,
                ..
            } => (object_shape_id, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        if *object_shape_id != shape_id {
            return Ok(false);
        }
        let Some(slot) = values.get_mut(usize::from(slot_index)) else {
            return Ok(false);
        };
        *slot = PropertyValue::data(value);
        Ok(true)
    }

    /// Defines or replaces an accessor property.
    pub fn define_accessor(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        getter: Option<ObjectHandle>,
        setter: Option<ObjectHandle>,
    ) -> Result<PropertyInlineCache, ObjectError> {
        let accessor = PropertyValue::accessor(getter, setter);

        if let Some(slot_index) = match self.object(handle)? {
            HeapValue::Object { keys, .. } => property_slot(keys, property),
            HeapValue::NativeObject { keys, .. } => property_slot(keys, property),
            HeapValue::Closure { keys, .. } => property_slot(keys, property),
            HeapValue::HostFunction { keys, .. } => property_slot(keys, property),
            HeapValue::BoundFunction { keys, .. } => property_slot(keys, property),
            HeapValue::ArrayBuffer { keys, .. } => property_slot(keys, property),
            HeapValue::SharedArrayBuffer { keys, .. } => property_slot(keys, property),
            HeapValue::DataView { keys, .. } => property_slot(keys, property),
            HeapValue::TypedArray { keys, .. } => property_slot(keys, property),
            HeapValue::RegExp { keys, .. } => property_slot(keys, property),
            HeapValue::Array { .. }
            | HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => return Err(ObjectError::InvalidKind),
        } {
            let object = self.object_mut(handle)?;
            let (shape_id, values) = match object {
                HeapValue::Object {
                    shape_id, values, ..
                }
                | HeapValue::NativeObject {
                    shape_id, values, ..
                }
                | HeapValue::Closure {
                    shape_id, values, ..
                }
                | HeapValue::HostFunction {
                    shape_id, values, ..
                }
                | HeapValue::BoundFunction {
                    shape_id, values, ..
                }
                | HeapValue::ArrayBuffer {
                    shape_id, values, ..
                }
                | HeapValue::SharedArrayBuffer {
                    shape_id, values, ..
                }
                | HeapValue::DataView {
                    shape_id, values, ..
                }
                | HeapValue::TypedArray {
                    shape_id, values, ..
                }
                | HeapValue::RegExp {
                    shape_id, values, ..
                } => (shape_id, values),
                _ => return Err(ObjectError::InvalidKind),
            };
            values[usize::from(slot_index)] = accessor;
            return Ok(PropertyInlineCache::new(*shape_id, slot_index));
        }

        let shape_id = self.allocate_shape();
        let object = self.object_mut(handle)?;
        let (object_shape_id, keys, values) = match object {
            HeapValue::Object {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::BoundFunction {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::SharedArrayBuffer {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::DataView {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            } => (object_shape_id, keys, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        keys.push(property);
        values.push(accessor);
        *object_shape_id = shape_id;
        let slot_index = u16::try_from(values.len().saturating_sub(1)).unwrap_or(u16::MAX);
        Ok(PropertyInlineCache::new(*object_shape_id, slot_index))
    }

    fn object(&self, handle: ObjectHandle) -> Result<&HeapValue, ObjectError> {
        self.heap
            .get::<HeapValue>(GcHandle(handle.0))
            .ok_or(ObjectError::InvalidHandle)
    }

    fn object_mut(&mut self, handle: ObjectHandle) -> Result<&mut HeapValue, ObjectError> {
        self.heap
            .get_mut::<HeapValue>(GcHandle(handle.0))
            .ok_or(ObjectError::InvalidHandle)
    }

    /// Returns all own property keys of an object-like heap value.
    ///
    /// For ordinary objects, returns named property keys in insertion order.
    /// For arrays, returns numeric indices as strings followed by named properties.
    pub fn own_keys(&self, handle: ObjectHandle) -> Result<Vec<PropertyNameId>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { keys, .. }
            | HeapValue::NativeObject { keys, .. }
            | HeapValue::Closure { keys, .. }
            | HeapValue::HostFunction { keys, .. }
            | HeapValue::BoundFunction { keys, .. }
            | HeapValue::ArrayBuffer { keys, .. }
            | HeapValue::SharedArrayBuffer { keys, .. }
            | HeapValue::DataView { keys, .. }
            | HeapValue::TypedArray { keys, .. }
            | HeapValue::RegExp { keys, .. } => Ok(keys.clone()),
            HeapValue::Array { keys, .. } => Ok(keys.clone()),
            _ => Ok(Vec::new()),
        }
    }

    /// Returns all own property keys, interning array index keys into the shared registry.
    pub fn own_keys_with_registry(
        &self,
        handle: ObjectHandle,
        property_names: &mut PropertyNameRegistry,
    ) -> Result<Vec<PropertyNameId>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { keys, .. }
            | HeapValue::NativeObject { keys, .. }
            | HeapValue::Closure { keys, .. }
            | HeapValue::HostFunction { keys, .. }
            | HeapValue::BoundFunction { keys, .. }
            | HeapValue::ArrayBuffer { keys, .. }
            | HeapValue::SharedArrayBuffer { keys, .. }
            | HeapValue::DataView { keys, .. }
            | HeapValue::TypedArray { keys, .. }
            | HeapValue::RegExp { keys, .. } => Ok(keys.clone()),
            HeapValue::Array {
                elements,
                indexed_properties,
                keys,
                ..
            } => {
                let mut result =
                    Vec::with_capacity(elements.len().saturating_add(keys.len()).saturating_add(1));
                for (index, value) in elements.iter().enumerate() {
                    if indexed_properties.contains_key(&index) {
                        let name = index.to_string();
                        result.push(property_names.intern(&name));
                        continue;
                    }
                    if value.is_hole() {
                        continue;
                    }
                    let name = index.to_string();
                    result.push(property_names.intern(&name));
                }
                for &index in indexed_properties.keys() {
                    if index >= elements.len() {
                        let name = index.to_string();
                        result.push(property_names.intern(&name));
                    }
                }
                result.push(property_names.intern("length"));
                result.extend(keys.iter().copied());
                Ok(result)
            }
            HeapValue::String { value, .. } => {
                let length = value.len();
                let mut keys = Vec::with_capacity(length.saturating_add(1));
                for index in 0..length {
                    let name = index.to_string();
                    keys.push(property_names.intern(&name));
                }
                keys.push(property_names.intern("length"));
                Ok(keys)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Returns an own property descriptor without walking the prototype chain.
    pub fn own_property_descriptor(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
        property_names: &PropertyNameRegistry,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        if let Some((value, _cache)) = self.get_own_property(handle, property, property_names)? {
            return Ok(Some(value));
        }
        Ok(None)
    }

    /// Checks if an object-like heap value has an own property with the given name.
    pub fn has_own_property(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { keys, .. }
            | HeapValue::NativeObject { keys, .. }
            | HeapValue::Closure { keys, .. }
            | HeapValue::HostFunction { keys, .. }
            | HeapValue::BoundFunction { keys, .. }
            | HeapValue::ArrayBuffer { keys, .. }
            | HeapValue::SharedArrayBuffer { keys, .. }
            | HeapValue::DataView { keys, .. }
            | HeapValue::TypedArray { keys, .. }
            | HeapValue::RegExp { keys, .. }
            | HeapValue::Array { keys, .. } => Ok(property_slot(keys, property).is_some()),
            _ => Ok(false),
        }
    }

    /// ES2024 §10.4.1.3 — Allocates a bound function exotic object.
    /// The `realm` is taken from the target via `function_realm`, falling back to
    /// the caller's current realm.
    pub fn alloc_bound_function(
        &mut self,
        target: ObjectHandle,
        bound_this: RegisterValue,
        bound_args: Vec<RegisterValue>,
        realm: crate::realm::RealmId,
    ) -> Result<ObjectHandle, ObjectError> {
        let prototype = self.get_prototype(target)?;
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::BoundFunction {
            prototype,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            target,
            bound_this,
            bound_args,
            realm,
        });
        Ok(ObjectHandle(h.0))
    }

    /// Returns the (target, bound_this, bound_args) for a bound function.
    pub fn bound_function_parts(
        &self,
        handle: ObjectHandle,
    ) -> Result<(ObjectHandle, RegisterValue, Vec<RegisterValue>), ObjectError> {
        match self.object(handle)? {
            HeapValue::BoundFunction {
                target,
                bound_this,
                bound_args,
                ..
            } => Ok((*target, *bound_this, bound_args.clone())),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// ES2024 §7.2.3 IsCallable — returns true if the value has a `[[Call]]` internal method.
    pub fn is_callable(&self, handle: ObjectHandle) -> bool {
        matches!(
            self.kind(handle),
            Ok(HeapValueKind::HostFunction
                | HeapValueKind::Closure
                | HeapValueKind::BoundFunction
                | HeapValueKind::PromiseCapabilityFunction
                | HeapValueKind::PromiseCombinatorElement
                | HeapValueKind::PromiseFinallyFunction
                | HeapValueKind::PromiseValueThunk)
        )
    }

    /// Returns the closure flags for a closure object.
    pub fn closure_flags(&self, handle: ObjectHandle) -> Result<ClosureFlags, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { flags, .. } => Ok(*flags),
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// ES2024 §10.1.3 `[[IsExtensible]]()` — returns the extensibility of an object.
    pub fn is_extensible(&self, handle: ObjectHandle) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { extensible, .. }
            | HeapValue::NativeObject { extensible, .. }
            | HeapValue::Array { extensible, .. }
            | HeapValue::Closure { extensible, .. }
            | HeapValue::HostFunction { extensible, .. }
            | HeapValue::BoundFunction { extensible, .. }
            | HeapValue::ArrayBuffer { extensible, .. }
            | HeapValue::SharedArrayBuffer { extensible, .. }
            | HeapValue::DataView { extensible, .. }
            | HeapValue::TypedArray { extensible, .. }
            | HeapValue::RegExp { extensible, .. } => Ok(*extensible),
            // Iterator objects are extensible (prototype must be settable during init).
            HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            // WeakMap/WeakSet/WeakRef/FinalizationRegistry are extensible.
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            // Generators are extensible (can have own properties).
            | HeapValue::Generator { .. } => Ok(true),
            // Strings, promises, upvalue cells are not extensible objects.
            _ => Ok(false),
        }
    }

    /// ES2024 §10.1.4 `[[PreventExtensions]]()` — marks an object as non-extensible.
    pub fn prevent_extensions(&mut self, handle: ObjectHandle) -> Result<bool, ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Object { extensible, .. }
            | HeapValue::NativeObject { extensible, .. }
            | HeapValue::Array { extensible, .. }
            | HeapValue::Closure { extensible, .. }
            | HeapValue::HostFunction { extensible, .. }
            | HeapValue::BoundFunction { extensible, .. }
            | HeapValue::ArrayBuffer { extensible, .. }
            | HeapValue::SharedArrayBuffer { extensible, .. }
            | HeapValue::DataView { extensible, .. }
            | HeapValue::TypedArray { extensible, .. }
            | HeapValue::RegExp { extensible, .. } => {
                *extensible = false;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// ES2024 §20.1.2.6 `Object.freeze(O)` — sets configurable+writable to false on all own properties.
    pub fn freeze(&mut self, handle: ObjectHandle) -> Result<(), ObjectError> {
        self.prevent_extensions(handle)?;
        if let HeapValue::Array {
            elements_writable,
            elements_configurable,
            length_writable,
            values,
            indexed_properties,
            ..
        } = self.object_mut(handle)?
        {
            *elements_writable = false;
            *elements_configurable = false;
            *length_writable = false;
            for value in values {
                match value {
                    PropertyValue::Data { attributes, .. } => {
                        *attributes = attributes.with_writable_false().with_configurable_false();
                    }
                    PropertyValue::Accessor { attributes, .. } => {
                        *attributes = attributes.with_configurable_false();
                    }
                }
            }
            for value in indexed_properties.values_mut() {
                match value {
                    PropertyValue::Data { attributes, .. } => {
                        *attributes = attributes.with_writable_false().with_configurable_false();
                    }
                    PropertyValue::Accessor { attributes, .. } => {
                        *attributes = attributes.with_configurable_false();
                    }
                }
            }
            return Ok(());
        }
        let keys = self.own_keys(handle)?;
        for key in keys {
            self.freeze_property(handle, key)?;
        }
        Ok(())
    }

    /// ES2024 §20.1.2.19 `Object.seal(O)` — sets configurable to false on all own properties.
    pub fn seal(&mut self, handle: ObjectHandle) -> Result<(), ObjectError> {
        self.prevent_extensions(handle)?;
        if let HeapValue::Array {
            elements_configurable,
            values,
            indexed_properties,
            ..
        } = self.object_mut(handle)?
        {
            *elements_configurable = false;
            for value in values {
                match value {
                    PropertyValue::Data { attributes, .. }
                    | PropertyValue::Accessor { attributes, .. } => {
                        *attributes = attributes.with_configurable_false();
                    }
                }
            }
            for value in indexed_properties.values_mut() {
                match value {
                    PropertyValue::Data { attributes, .. }
                    | PropertyValue::Accessor { attributes, .. } => {
                        *attributes = attributes.with_configurable_false();
                    }
                }
            }
            return Ok(());
        }
        let keys = self.own_keys(handle)?;
        for key in keys {
            self.seal_property(handle, key)?;
        }
        Ok(())
    }

    /// ES2024 integrity-level check used by `Object.isSealed`.
    pub fn is_sealed(&self, handle: ObjectHandle) -> Result<bool, ObjectError> {
        if self.is_extensible(handle)? {
            return Ok(false);
        }

        let values = match self.object(handle)? {
            HeapValue::Object { values, .. }
            | HeapValue::NativeObject { values, .. }
            | HeapValue::Closure { values, .. }
            | HeapValue::HostFunction { values, .. }
            | HeapValue::BoundFunction { values, .. }
            | HeapValue::ArrayBuffer { values, .. }
            | HeapValue::SharedArrayBuffer { values, .. }
            | HeapValue::DataView { values, .. }
            | HeapValue::TypedArray { values, .. }
            | HeapValue::RegExp { values, .. } => values,
            HeapValue::Array {
                elements_configurable,
                values,
                indexed_properties,
                ..
            } => {
                return Ok(!elements_configurable
                    && values
                        .iter()
                        .all(|value| !value.attributes().configurable())
                    && indexed_properties
                        .values()
                        .all(|value| !value.attributes().configurable()));
            }
            _ => return Ok(true),
        };

        Ok(values
            .iter()
            .all(|value| !value.attributes().configurable()))
    }

    /// ES2024 integrity-level check used by `Object.isFrozen`.
    pub fn is_frozen(&self, handle: ObjectHandle) -> Result<bool, ObjectError> {
        if self.is_extensible(handle)? {
            return Ok(false);
        }

        let values = match self.object(handle)? {
            HeapValue::Object { values, .. }
            | HeapValue::NativeObject { values, .. }
            | HeapValue::Closure { values, .. }
            | HeapValue::HostFunction { values, .. }
            | HeapValue::BoundFunction { values, .. }
            | HeapValue::ArrayBuffer { values, .. }
            | HeapValue::SharedArrayBuffer { values, .. }
            | HeapValue::DataView { values, .. }
            | HeapValue::TypedArray { values, .. }
            | HeapValue::RegExp { values, .. } => values,
            HeapValue::Array {
                elements_writable,
                elements_configurable,
                length_writable,
                values,
                indexed_properties,
                ..
            } => {
                return Ok(!elements_configurable
                    && !elements_writable
                    && !length_writable
                    && values.iter().all(|value| match value {
                        PropertyValue::Data { attributes, .. } => {
                            !attributes.configurable() && !attributes.writable()
                        }
                        PropertyValue::Accessor { attributes, .. } => !attributes.configurable(),
                    })
                    && indexed_properties.values().all(|value| match value {
                        PropertyValue::Data { attributes, .. } => {
                            !attributes.configurable() && !attributes.writable()
                        }
                        PropertyValue::Accessor { attributes, .. } => !attributes.configurable(),
                    }));
            }
            _ => return Ok(true),
        };

        Ok(values.iter().all(|value| match value {
            PropertyValue::Data { attributes, .. } => {
                !attributes.configurable() && !attributes.writable()
            }
            PropertyValue::Accessor { attributes, .. } => !attributes.configurable(),
        }))
    }

    /// Sets configurable=false and writable=false on a single property.
    fn freeze_property(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<(), ObjectError> {
        let (keys, values) = match self.object_mut(handle)? {
            HeapValue::Object { keys, values, .. }
            | HeapValue::NativeObject { keys, values, .. }
            | HeapValue::Closure { keys, values, .. }
            | HeapValue::HostFunction { keys, values, .. }
            | HeapValue::BoundFunction { keys, values, .. }
            | HeapValue::ArrayBuffer { keys, values, .. }
            | HeapValue::SharedArrayBuffer { keys, values, .. }
            | HeapValue::DataView { keys, values, .. }
            | HeapValue::TypedArray { keys, values, .. }
            | HeapValue::RegExp { keys, values, .. } => (keys, values),
            _ => return Ok(()),
        };
        if let Some(slot) = property_slot(keys, property).map(usize::from) {
            match &mut values[slot] {
                PropertyValue::Data { attributes, .. } => {
                    *attributes = attributes.with_writable_false().with_configurable_false();
                }
                PropertyValue::Accessor { attributes, .. } => {
                    *attributes = attributes.with_configurable_false();
                }
            }
        }
        Ok(())
    }

    /// Sets configurable=false on a single property.
    fn seal_property(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<(), ObjectError> {
        let (keys, values) = match self.object_mut(handle)? {
            HeapValue::Object { keys, values, .. }
            | HeapValue::NativeObject { keys, values, .. }
            | HeapValue::Closure { keys, values, .. }
            | HeapValue::HostFunction { keys, values, .. }
            | HeapValue::BoundFunction { keys, values, .. }
            | HeapValue::ArrayBuffer { keys, values, .. }
            | HeapValue::SharedArrayBuffer { keys, values, .. }
            | HeapValue::DataView { keys, values, .. }
            | HeapValue::TypedArray { keys, values, .. }
            | HeapValue::RegExp { keys, values, .. } => (keys, values),
            _ => return Ok(()),
        };
        if let Some(slot) = property_slot(keys, property).map(usize::from) {
            match &mut values[slot] {
                PropertyValue::Data { attributes, .. }
                | PropertyValue::Accessor { attributes, .. } => {
                    *attributes = attributes.with_configurable_false();
                }
            }
        }
        Ok(())
    }

    fn new_property_from_descriptor(&self, desc: PropertyDescriptor) -> PropertyValue {
        match desc.kind() {
            PropertyDescriptorKind::Accessor { getter, setter } => PropertyValue::Accessor {
                getter: getter.unwrap_or(None),
                setter: setter.unwrap_or(None),
                attributes: PropertyAttributes::from_flags(
                    false,
                    desc.enumerable().unwrap_or(false),
                    desc.configurable().unwrap_or(false),
                ),
            },
            PropertyDescriptorKind::Generic => PropertyValue::Data {
                value: RegisterValue::undefined(),
                attributes: PropertyAttributes::from_flags(
                    false,
                    desc.enumerable().unwrap_or(false),
                    desc.configurable().unwrap_or(false),
                ),
            },
            PropertyDescriptorKind::Data { value, writable } => PropertyValue::Data {
                value: value.unwrap_or_else(RegisterValue::undefined),
                attributes: PropertyAttributes::from_flags(
                    writable.unwrap_or(false),
                    desc.enumerable().unwrap_or(false),
                    desc.configurable().unwrap_or(false),
                ),
            },
        }
    }

    fn apply_property_descriptor(
        &self,
        current: PropertyValue,
        desc: PropertyDescriptor,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        match current {
            PropertyValue::Data {
                value: current_value,
                attributes,
            } => self.apply_data_property_descriptor(current_value, attributes, desc),
            PropertyValue::Accessor {
                getter: current_getter,
                setter: current_setter,
                attributes,
            } => self.apply_accessor_property_descriptor(
                current_getter,
                current_setter,
                attributes,
                desc,
            ),
        }
    }

    fn apply_data_property_descriptor(
        &self,
        current_value: RegisterValue,
        current_attributes: PropertyAttributes,
        desc: PropertyDescriptor,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        if !current_attributes.configurable() {
            if desc.configurable() == Some(true) {
                return Ok(None);
            }
            if let Some(enumerable) = desc.enumerable()
                && enumerable != current_attributes.enumerable()
            {
                return Ok(None);
            }
        }

        match desc.kind() {
            PropertyDescriptorKind::Accessor { getter, setter } => {
                if !current_attributes.configurable() {
                    return Ok(None);
                }
                Ok(Some(PropertyValue::Accessor {
                    getter: getter.unwrap_or(None),
                    setter: setter.unwrap_or(None),
                    attributes: PropertyAttributes::from_flags(
                        false,
                        desc.enumerable().unwrap_or(current_attributes.enumerable()),
                        desc.configurable()
                            .unwrap_or(current_attributes.configurable()),
                    ),
                }))
            }
            PropertyDescriptorKind::Generic
            | PropertyDescriptorKind::Data {
                value: _,
                writable: _,
            } => {
                let (next_value, next_writable) = match desc.kind() {
                    PropertyDescriptorKind::Generic => {
                        (current_value, current_attributes.writable())
                    }
                    PropertyDescriptorKind::Data { value, writable } => (
                        value.unwrap_or(current_value),
                        writable.unwrap_or(current_attributes.writable()),
                    ),
                    PropertyDescriptorKind::Accessor { .. } => unreachable!(),
                };

                if !current_attributes.configurable()
                    && !current_attributes.writable()
                    && let PropertyDescriptorKind::Data { value, writable } = desc.kind()
                {
                    if writable == Some(true) {
                        return Ok(None);
                    }
                    if let Some(value) = value
                        && !self.same_value(value, current_value)?
                    {
                        return Ok(None);
                    }
                }

                Ok(Some(PropertyValue::Data {
                    value: next_value,
                    attributes: PropertyAttributes::from_flags(
                        next_writable,
                        desc.enumerable().unwrap_or(current_attributes.enumerable()),
                        desc.configurable()
                            .unwrap_or(current_attributes.configurable()),
                    ),
                }))
            }
        }
    }

    fn apply_accessor_property_descriptor(
        &self,
        current_getter: Option<ObjectHandle>,
        current_setter: Option<ObjectHandle>,
        current_attributes: PropertyAttributes,
        desc: PropertyDescriptor,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        if !current_attributes.configurable() {
            if desc.configurable() == Some(true) {
                return Ok(None);
            }
            if let Some(enumerable) = desc.enumerable()
                && enumerable != current_attributes.enumerable()
            {
                return Ok(None);
            }
        }

        match desc.kind() {
            PropertyDescriptorKind::Data { value, writable } => {
                if !current_attributes.configurable() {
                    return Ok(None);
                }
                Ok(Some(PropertyValue::Data {
                    value: value.unwrap_or_else(RegisterValue::undefined),
                    attributes: PropertyAttributes::from_flags(
                        writable.unwrap_or(false),
                        desc.enumerable().unwrap_or(current_attributes.enumerable()),
                        desc.configurable()
                            .unwrap_or(current_attributes.configurable()),
                    ),
                }))
            }
            PropertyDescriptorKind::Generic => Ok(Some(PropertyValue::Accessor {
                getter: current_getter,
                setter: current_setter,
                attributes: PropertyAttributes::from_flags(
                    false,
                    desc.enumerable().unwrap_or(current_attributes.enumerable()),
                    desc.configurable()
                        .unwrap_or(current_attributes.configurable()),
                ),
            })),
            PropertyDescriptorKind::Accessor { getter, setter } => {
                if !current_attributes.configurable() {
                    if let Some(getter) = getter
                        && getter != current_getter
                    {
                        return Ok(None);
                    }
                    if let Some(setter) = setter
                        && setter != current_setter
                    {
                        return Ok(None);
                    }
                }
                Ok(Some(PropertyValue::Accessor {
                    getter: getter.unwrap_or(current_getter),
                    setter: setter.unwrap_or(current_setter),
                    attributes: PropertyAttributes::from_flags(
                        false,
                        desc.enumerable().unwrap_or(current_attributes.enumerable()),
                        desc.configurable()
                            .unwrap_or(current_attributes.configurable()),
                    ),
                }))
            }
        }
    }

    fn set_named_property_storage(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<PropertyInlineCache, ObjectError> {
        if let Some(slot_index) = match self.object(handle)? {
            HeapValue::Object { keys, .. }
            | HeapValue::NativeObject { keys, .. }
            | HeapValue::Closure { keys, .. }
            | HeapValue::HostFunction { keys, .. }
            | HeapValue::BoundFunction { keys, .. }
            | HeapValue::Array { keys, .. }
            | HeapValue::ArrayBuffer { keys, .. }
            | HeapValue::SharedArrayBuffer { keys, .. }
            | HeapValue::DataView { keys, .. }
            | HeapValue::TypedArray { keys, .. }
            | HeapValue::RegExp { keys, .. } => property_slot(keys, property),
            HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::Proxy { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. } => {
                return Err(ObjectError::InvalidKind);
            }
        } {
            let object = self.object_mut(handle)?;
            let (shape_id, values) = match object {
                HeapValue::Object {
                    shape_id, values, ..
                }
                | HeapValue::NativeObject {
                    shape_id, values, ..
                }
                | HeapValue::Closure {
                    shape_id, values, ..
                }
                | HeapValue::HostFunction {
                    shape_id, values, ..
                }
                | HeapValue::BoundFunction {
                    shape_id, values, ..
                }
                | HeapValue::Array {
                    shape_id, values, ..
                }
                | HeapValue::ArrayBuffer {
                    shape_id, values, ..
                }
                | HeapValue::SharedArrayBuffer {
                    shape_id, values, ..
                }
                | HeapValue::DataView {
                    shape_id, values, ..
                }
                | HeapValue::RegExp {
                    shape_id, values, ..
                } => (shape_id, values),
                _ => return Err(ObjectError::InvalidKind),
            };
            let slot = &mut values[usize::from(slot_index)];
            match slot {
                PropertyValue::Data { attributes, .. } if !attributes.writable() => {
                    return Ok(PropertyInlineCache::new(*shape_id, slot_index));
                }
                PropertyValue::Data { value: v, .. } => {
                    *v = value;
                }
                PropertyValue::Accessor { .. } => {
                    return Ok(PropertyInlineCache::new(*shape_id, slot_index));
                }
            }
            return Ok(PropertyInlineCache::new(*shape_id, slot_index));
        }

        if !self.is_extensible(handle)? {
            let shape_id = match self.object(handle)? {
                HeapValue::Object { shape_id, .. }
                | HeapValue::NativeObject { shape_id, .. }
                | HeapValue::Closure { shape_id, .. }
                | HeapValue::HostFunction { shape_id, .. }
                | HeapValue::BoundFunction { shape_id, .. }
                | HeapValue::Array { shape_id, .. }
                | HeapValue::ArrayBuffer { shape_id, .. }
                | HeapValue::SharedArrayBuffer { shape_id, .. }
                | HeapValue::DataView { shape_id, .. }
                | HeapValue::TypedArray { shape_id, .. }
                | HeapValue::RegExp { shape_id, .. } => *shape_id,
                _ => return Err(ObjectError::InvalidKind),
            };
            return Ok(PropertyInlineCache::new(shape_id, 0));
        }

        let shape_id = self.allocate_shape();
        let object = self.object_mut(handle)?;
        let (object_shape_id, keys, values) = match object {
            HeapValue::Object {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::BoundFunction {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::Array {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::SharedArrayBuffer {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::DataView {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            } => (object_shape_id, keys, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        keys.push(property);
        values.push(PropertyValue::data(value));
        *object_shape_id = shape_id;

        let slot_index = u16::try_from(values.len().saturating_sub(1)).unwrap_or(u16::MAX);
        Ok(PropertyInlineCache::new(*object_shape_id, slot_index))
    }

    fn set_array_named_property_storage(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<PropertyInlineCache, ObjectError> {
        self.set_named_property_storage(handle, property, value)
    }

    fn define_named_property_storage(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        desc: PropertyDescriptor,
        include_array: bool,
    ) -> Result<bool, ObjectError> {
        let existing_slot = match self.object(handle)? {
            HeapValue::Object { keys, .. }
            | HeapValue::NativeObject { keys, .. }
            | HeapValue::Closure { keys, .. }
            | HeapValue::HostFunction { keys, .. } => property_slot(keys, property),
            HeapValue::BoundFunction { keys, .. } => property_slot(keys, property),
            HeapValue::ArrayBuffer { keys, .. } => property_slot(keys, property),
            HeapValue::SharedArrayBuffer { keys, .. } => property_slot(keys, property),
            HeapValue::RegExp { keys, .. } => property_slot(keys, property),
            HeapValue::Array { keys, .. } if include_array => property_slot(keys, property),
            _ => None,
        };

        if let Some(slot_index) = existing_slot {
            let slot = usize::from(slot_index);
            let existing = {
                let values = match self.object(handle)? {
                    HeapValue::Object { values, .. }
                    | HeapValue::NativeObject { values, .. }
                    | HeapValue::Closure { values, .. }
                    | HeapValue::HostFunction { values, .. }
                    | HeapValue::BoundFunction { values, .. }
                    | HeapValue::ArrayBuffer { values, .. }
                    | HeapValue::SharedArrayBuffer { values, .. }
                    | HeapValue::DataView { values, .. }
                    | HeapValue::TypedArray { values, .. }
                    | HeapValue::RegExp { values, .. } => values,
                    HeapValue::Array { values, .. } if include_array => values,
                    _ => return Err(ObjectError::InvalidKind),
                };
                values[slot]
            };

            let Some(next_value) = self.apply_property_descriptor(existing, desc)? else {
                return Ok(false);
            };

            let values = match self.object_mut(handle)? {
                HeapValue::Object { values, .. }
                | HeapValue::NativeObject { values, .. }
                | HeapValue::Closure { values, .. }
                | HeapValue::HostFunction { values, .. }
                | HeapValue::BoundFunction { values, .. }
                | HeapValue::ArrayBuffer { values, .. }
                | HeapValue::SharedArrayBuffer { values, .. }
                | HeapValue::DataView { values, .. }
                | HeapValue::TypedArray { values, .. }
                | HeapValue::RegExp { values, .. } => values,
                HeapValue::Array { values, .. } if include_array => values,
                _ => return Err(ObjectError::InvalidKind),
            };
            values[slot] = next_value;
            return Ok(true);
        }

        if !self.is_extensible(handle)? {
            return Ok(false);
        }

        let next_value = self.new_property_from_descriptor(desc);

        let shape_id = self.allocate_shape();
        let object = self.object_mut(handle)?;
        let (object_shape_id, keys, values) = match object {
            HeapValue::Object {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::BoundFunction {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::SharedArrayBuffer {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::DataView {
                shape_id: s,
                keys,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id: s,
                keys,
                values,
                ..
            } => (s, keys, values),
            HeapValue::Array {
                shape_id: s,
                keys,
                values,
                ..
            } if include_array => (s, keys, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        keys.push(property);
        values.push(next_value);
        *object_shape_id = shape_id;
        Ok(true)
    }

    fn delete_named_property_storage(
        &mut self,
        handle: ObjectHandle,
        property: PropertyNameId,
        include_array: bool,
    ) -> Result<bool, ObjectError> {
        let slot_index = match self.object(handle)? {
            HeapValue::Object { keys, .. }
            | HeapValue::NativeObject { keys, .. }
            | HeapValue::Closure { keys, .. }
            | HeapValue::HostFunction { keys, .. }
            | HeapValue::BoundFunction { keys, .. }
            | HeapValue::ArrayBuffer { keys, .. } => property_slot(keys, property),
            HeapValue::SharedArrayBuffer { keys, .. } => property_slot(keys, property),
            HeapValue::Array { keys, .. } if include_array => property_slot(keys, property),
            HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. } => return Err(ObjectError::InvalidKind),
            _ => None,
        };

        let Some(slot_index) = slot_index.map(usize::from) else {
            return Ok(true);
        };

        {
            let values = match self.object(handle)? {
                HeapValue::Object { values, .. }
                | HeapValue::NativeObject { values, .. }
                | HeapValue::Closure { values, .. }
                | HeapValue::HostFunction { values, .. }
                | HeapValue::BoundFunction { values, .. } => values,
                HeapValue::Array { values, .. } if include_array => values,
                _ => return Err(ObjectError::InvalidKind),
            };
            if !values[slot_index].attributes().configurable() {
                return Ok(false);
            }
        }

        let shape_id = self.allocate_shape();
        let object = self.object_mut(handle)?;
        let (object_shape_id, keys, values) = match object {
            HeapValue::Object {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::BoundFunction {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            } => (object_shape_id, keys, values),
            HeapValue::Array {
                shape_id: object_shape_id,
                keys,
                values,
                ..
            } if include_array => (object_shape_id, keys, values),
            _ => return Err(ObjectError::InvalidKind),
        };

        keys.remove(slot_index);
        values.remove(slot_index);
        *object_shape_id = shape_id;
        Ok(true)
    }

    fn set_array_length_from_value(
        &mut self,
        handle: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        let (current_length, length_writable) = match self.object(handle)? {
            HeapValue::Array {
                elements,
                length_writable,
                ..
            } => (elements.len(), *length_writable),
            _ => return Err(ObjectError::InvalidKind),
        };
        let Some(next_length) = array_length_from_value(value) else {
            return Err(ObjectError::InvalidArrayLength);
        };

        if next_length == current_length || !length_writable {
            return Ok(());
        }

        self.set_array_length(handle, next_length)?;
        Ok(())
    }

    fn allocate_shape(&mut self) -> ObjectShapeId {
        let shape_id = ObjectShapeId(self.next_shape_id);
        self.next_shape_id = self.next_shape_id.saturating_add(1);
        shape_id
    }

    /// Triggers garbage collection with ephemeron support for WeakMap/WeakSet.
    ///
    /// Phases: mark → ephemeron fixpoint → clear dead weak entries → sweep.
    /// Spec: <https://tc39.es/ecma262/#sec-weakref-processing-model>
    pub fn collect_garbage(&mut self, roots: &[ObjectHandle]) {
        let gc_roots: Vec<GcHandle> = roots.iter().map(|h| GcHandle(h.0)).collect();

        // Phase 1: Mark from roots (WeakMap/WeakSet entries are NOT traced).
        self.heap.run_mark_phase(&gc_roots);

        // Phase 2: Ephemeron fixpoint — trace values of surviving weak entries.
        let weakmap_handles: Vec<ObjectHandle> = self.find_weak_handles(HeapValueKind::WeakMap);
        if !weakmap_handles.is_empty() {
            loop {
                let mut extra: Vec<GcHandle> = Vec::new();
                for &wm in &weakmap_handles {
                    if !self.heap.is_marked(GcHandle(wm.0)) {
                        continue;
                    }
                    if let Ok(HeapValue::WeakMap { entries, .. }) = self.object(wm) {
                        for (&key, value) in entries {
                            if self.heap.is_marked(GcHandle(key))
                                && let Some(vh) = value.as_object_handle()
                                && !self.heap.is_marked(GcHandle(vh))
                            {
                                extra.push(GcHandle(vh));
                            }
                        }
                    }
                }
                if extra.is_empty() {
                    break;
                }
                self.heap.run_mark_additional(&extra);
            }
        }

        // Phase 3: Clear dead weak entries.
        // Copy marks to break borrow — marks are small (1 byte per object).
        let marks: Vec<bool> = self.heap.marks().to_vec();
        let is_marked = |h: u32| marks.get(h as usize).copied().unwrap_or(false);
        for &wm in &weakmap_handles {
            if self.heap.is_marked(GcHandle(wm.0)) {
                let _ = self.weakmap_clear_dead_and_get_live_values(wm, &is_marked);
            }
        }
        let weakset_handles: Vec<ObjectHandle> = self.find_weak_handles(HeapValueKind::WeakSet);
        for &ws in &weakset_handles {
            if self.heap.is_marked(GcHandle(ws.0)) {
                let _ = self.weakset_clear_dead(ws, &is_marked);
            }
        }

        // Phase 3b: Clear dead WeakRef targets.
        let weakref_handles: Vec<ObjectHandle> = self.find_weak_handles(HeapValueKind::WeakRef);
        for &wr in &weakref_handles {
            if self.heap.is_marked(GcHandle(wr.0)) {
                let _ = self.weakref_clear_dead(wr, &is_marked);
            }
        }

        // Phase 3c: Clear dead FinalizationRegistry cells.
        // Held values from dead cells are collected for later cleanup callback invocation.
        let fr_handles: Vec<ObjectHandle> =
            self.find_weak_handles(HeapValueKind::FinalizationRegistry);
        for &fr in &fr_handles {
            if self.heap.is_marked(GcHandle(fr.0)) {
                // Note: cleanup callbacks are deferred to the microtask queue.
                // For now we just clear dead cells — the held values are discarded.
                // Full spec compliance requires queueing HostCleanupFinalizationRegistry jobs.
                let _ = self.finalization_registry_clear_dead(fr, &is_marked);
            }
        }

        // Phase 4: Sweep.
        self.heap.run_sweep_phase();
    }

    /// Triggers GC if memory pressure warrants it.
    pub fn maybe_collect_garbage(&mut self, roots: &[ObjectHandle]) {
        let gc_roots: Vec<GcHandle> = roots.iter().map(|h| GcHandle(h.0)).collect();
        self.heap.maybe_collect(&gc_roots);
    }

    /// Finds all handles of a given HeapValueKind (for ephemeron processing).
    fn find_weak_handles(&self, target_kind: HeapValueKind) -> Vec<ObjectHandle> {
        let mut handles = Vec::new();
        self.heap.for_each(|idx, _any| {
            let h = ObjectHandle(idx);
            if self.kind(h) == Ok(target_kind) {
                handles.push(h);
            }
        });
        handles
    }

    /// Returns the number of live objects.
    pub fn live_count(&self) -> usize {
        self.heap.live_count()
    }

    fn get_own_property(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
        property_names: &PropertyNameRegistry,
    ) -> Result<Option<(PropertyValue, PropertyInlineCache)>, ObjectError> {
        let object = self.object(handle)?;
        let (shape_id, keys, values) = match object {
            HeapValue::Object {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::NativeObject {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::Closure {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::HostFunction {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::BoundFunction {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::ArrayBuffer {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::SharedArrayBuffer {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::DataView {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::TypedArray {
                shape_id,
                keys,
                values,
                ..
            }
            | HeapValue::RegExp {
                shape_id,
                keys,
                values,
                ..
            } => (shape_id, keys, values),
            HeapValue::Array {
                shape_id,
                keys,
                values,
                elements,
                indexed_properties,
                elements_writable,
                elements_configurable,
                length_writable,
                ..
            } => {
                let Some(property_name) = property_names.get(property) else {
                    return Ok(None);
                };
                if property_name == "length" {
                    return Ok(Some((
                        PropertyValue::data_with_attrs(
                            RegisterValue::from_i32(
                                i32::try_from(elements.len()).unwrap_or(i32::MAX),
                            ),
                            PropertyAttributes::from_flags(*length_writable, false, false),
                        ),
                        PropertyInlineCache::new(ObjectShapeId(0), 0),
                    )));
                }

                if let Some(index) = canonical_array_index(property_name) {
                    if let Some(value) = indexed_properties.get(&index).copied() {
                        return Ok(Some((value, PropertyInlineCache::new(ObjectShapeId(0), 0))));
                    }
                    let Some(value) = elements.get(index).copied() else {
                        return Ok(None);
                    };
                    if value.is_hole() {
                        return Ok(None);
                    }
                    return Ok(Some((
                        PropertyValue::data_with_attrs(
                            value,
                            PropertyAttributes::from_flags(
                                *elements_writable,
                                true,
                                *elements_configurable,
                            ),
                        ),
                        PropertyInlineCache::new(ObjectShapeId(0), 0),
                    )));
                }

                if let Some(slot_index) = property_slot(keys, property) {
                    let slot_index = usize::from(slot_index);
                    if let Some(value) = values.get(slot_index).copied() {
                        return Ok(Some((
                            value,
                            PropertyInlineCache::new(*shape_id, slot_index as u16),
                        )));
                    }
                }
                return Ok(None);
            }
            HeapValue::String { value, .. } => {
                let Some(property_name) = property_names.get(property) else {
                    return Ok(None);
                };
                if property_name == "length" {
                    return Ok(Some((
                        PropertyValue::data_with_attrs(
                            RegisterValue::from_i32(i32::try_from(value.len()).unwrap_or(i32::MAX)),
                            PropertyAttributes::from_flags(false, false, false),
                        ),
                        PropertyInlineCache::new(ObjectShapeId(0), 0),
                    )));
                }
                return Ok(None);
            }
            HeapValue::UpvalueCell { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. } => return Err(ObjectError::InvalidKind),
            // Objects with no own named properties but participate in prototype chain.
            HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. } => return Ok(None),
            HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. }
            | HeapValue::WeakRef { .. }
            | HeapValue::FinalizationRegistry { .. }
            | HeapValue::Generator { .. }
            | HeapValue::AsyncGenerator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::Proxy { .. } => return Ok(None),
        };
        let Some(slot_index) = property_slot(keys, property) else {
            return Ok(None);
        };
        let value = values[usize::from(slot_index)];
        let cache = PropertyInlineCache::new(*shape_id, slot_index);
        Ok(Some((value, cache)))
    }

    fn property_traversal_prototype(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { prototype, .. }
            | HeapValue::NativeObject { prototype, .. }
            | HeapValue::Array { prototype, .. }
            | HeapValue::String { prototype, .. }
            | HeapValue::Closure { prototype, .. }
            | HeapValue::HostFunction { prototype, .. }
            | HeapValue::BoundFunction { prototype, .. }
            | HeapValue::Map { prototype, .. }
            | HeapValue::Set { prototype, .. }
            | HeapValue::ArrayIterator { prototype, .. }
            | HeapValue::StringIterator { prototype, .. }
            | HeapValue::MapIterator { prototype, .. }
            | HeapValue::SetIterator { prototype, .. }
            | HeapValue::WeakMap { prototype, .. }
            | HeapValue::WeakSet { prototype, .. }
            | HeapValue::WeakRef { prototype, .. }
            | HeapValue::FinalizationRegistry { prototype, .. }
            | HeapValue::Generator { prototype, .. }
            | HeapValue::AsyncGenerator { prototype, .. }
            | HeapValue::Promise { prototype, .. }
            | HeapValue::ArrayBuffer { prototype, .. }
            | HeapValue::SharedArrayBuffer { prototype, .. }
            | HeapValue::DataView { prototype, .. }
            | HeapValue::TypedArray { prototype, .. }
            | HeapValue::RegExp { prototype, .. } => Ok(*prototype),
            HeapValue::Proxy {
                target,
                revoked: false,
                ..
            } => {
                let target = *target;
                self.property_traversal_prototype(target)
            }
            HeapValue::Proxy { revoked: true, .. } => Err(ObjectError::InvalidKind),
            HeapValue::UpvalueCell { .. }
            | HeapValue::BigInt { .. }
            | HeapValue::ErrorStackFrames { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::PromiseCapabilityFunction { .. }
            | HeapValue::PromiseCombinatorElement { .. }
            | HeapValue::PromiseFinallyFunction { .. }
            | HeapValue::PromiseValueThunk { .. } => Err(ObjectError::InvalidKind),
        }
    }
}

fn property_slot(keys: &[PropertyNameId], property: PropertyNameId) -> Option<u16> {
    keys.iter()
        .position(|key| *key == property)
        .and_then(|index| u16::try_from(index).ok())
}

fn canonical_array_index(property_name: &str) -> Option<usize> {
    let index = property_name.parse::<u32>().ok()?;
    if index == u32::MAX || index.to_string() != property_name {
        return None;
    }
    Some(index as usize)
}

fn array_index_default_attributes(
    elements_writable: bool,
    elements_configurable: bool,
) -> PropertyAttributes {
    PropertyAttributes::from_flags(elements_writable, true, elements_configurable)
}

fn array_length_from_value(value: RegisterValue) -> Option<usize> {
    if let Some(length) = value.as_i32()
        && length >= 0
    {
        return Some(length as usize);
    }

    let length = value.as_number()?;
    if !length.is_finite() || length < 0.0 || length.fract() != 0.0 {
        return None;
    }
    if length > (u32::MAX - 1) as f64 || length > usize::MAX as f64 {
        return None;
    }
    Some(length as usize)
}

/// SameValueZero comparison at the RegisterValue level.
/// For string comparison, delegates to the heap; other primitives compare by bits.
fn svz(heap: &TypedHeap, a: RegisterValue, b: RegisterValue) -> bool {
    if let (Some(na), Some(nb)) = (a.as_number(), b.as_number()) {
        if na.is_nan() && nb.is_nan() {
            return true;
        }
        return na == nb;
    }
    if a == b {
        return true;
    }
    // Different handles might refer to equal strings.
    if let (Some(ah), Some(bh)) = (a.as_object_handle(), b.as_object_handle())
        && let (Some(hva), Some(hvb)) = (
            heap.get::<HeapValue>(GcHandle(ah)),
            heap.get::<HeapValue>(GcHandle(bh)),
        )
        && let (HeapValue::String { value: sa, .. }, HeapValue::String { value: sb, .. }) =
            (hva, hvb)
    {
        return sa == sb;
    }
    false
}

impl ObjectHeap {
    /// Find the index of a matching Map entry by key using SameValueZero.
    fn map_find_index(
        &self,
        handle: ObjectHandle,
        key: RegisterValue,
    ) -> Result<Option<usize>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => {
                for (i, entry) in entries.iter().enumerate() {
                    if let Some(e) = entry
                        && svz(&self.heap, e.0, key)
                    {
                        return Ok(Some(i));
                    }
                }
                Ok(None)
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Find the index of a matching Set entry by value using SameValueZero.
    fn set_find_index(
        &self,
        handle: ObjectHandle,
        value: RegisterValue,
    ) -> Result<Option<usize>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => {
                for (i, entry) in entries.iter().enumerate() {
                    if let Some(e) = entry
                        && svz(&self.heap, *e, value)
                    {
                        return Ok(Some(i));
                    }
                }
                Ok(None)
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }
}

/// Normalize -0.0 to +0.0 per ES2024 §24.1.3.9 step 6.
fn normalize_zero(v: RegisterValue) -> RegisterValue {
    if let Some(n) = v.as_number()
        && n == 0.0
        && n.is_sign_negative()
    {
        return RegisterValue::from_number(0.0);
    }
    v
}

#[cfg(test)]
mod tests {
    use crate::host::HostFunctionId;
    use crate::module::FunctionIndex;
    use crate::property::{PropertyNameId, PropertyNameRegistry};
    use crate::value::RegisterValue;

    use super::ClosureFlags;
    use crate::payload::NativePayloadId;

    use super::{
        HeapValueKind, IteratorStep, ObjectError, ObjectHeap, PropertyAttributes,
        PropertyDescriptor, PropertyInlineCache, PropertyLookup, PropertyValue,
    };

    #[test]
    fn object_heap_supports_generic_and_cached_property_access() {
        let mut heap = ObjectHeap::new();
        let handle = heap.alloc_object();
        let property = PropertyNameId(0);

        let cache = heap
            .set_property(handle, property, RegisterValue::from_i32(7))
            .expect("object handle should be valid");
        assert_eq!(
            heap.get_cached(handle, property, cache)
                .expect("cache lookup should succeed"),
            Some(PropertyValue::data(RegisterValue::from_i32(7)))
        );

        assert!(
            heap.set_cached(handle, property, RegisterValue::from_i32(9), cache)
                .expect("cache store should succeed")
        );

        let generic = heap
            .get_property(handle, property)
            .expect("generic lookup should succeed");
        assert_eq!(
            generic.map(|lookup| {
                let PropertyValue::Data { value, .. } = lookup.value() else {
                    panic!("expected data property");
                };
                (value, lookup.cache())
            }),
            Some((
                RegisterValue::from_i32(9),
                Some(PropertyInlineCache::new(
                    cache.shape_id(),
                    cache.slot_index()
                ))
            ))
        );
    }

    #[test]
    fn cached_property_store_preserves_non_writable_attributes() {
        let mut heap = ObjectHeap::new();
        let handle = heap.alloc_object();
        let property = PropertyNameId(0);

        assert!(
            heap.define_own_property(
                handle,
                property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(7),
                    PropertyAttributes::constant(),
                ),
            )
            .expect("define should succeed")
        );

        let PropertyLookup { cache, value, .. } = heap
            .get_property(handle, property)
            .expect("lookup should succeed")
            .expect("property should exist");
        let PropertyValue::Data { attributes, .. } = value else {
            panic!("expected data property");
        };
        assert!(!attributes.writable(), "fixture should be non-writable");

        let cache = cache.expect("own property should expose inline cache");
        assert!(
            !heap
                .set_cached(handle, property, RegisterValue::from_i32(9), cache)
                .expect("cache store should succeed"),
            "cached store should report failure for non-writable property"
        );

        let PropertyLookup { value, .. } = heap
            .get_property(handle, property)
            .expect("lookup should succeed")
            .expect("property should still exist");
        let PropertyValue::Data {
            value, attributes, ..
        } = value
        else {
            panic!("expected data property");
        };
        assert_eq!(value, RegisterValue::from_i32(7));
        assert!(
            !attributes.writable(),
            "non-writable flag should be preserved"
        );
        assert!(
            !attributes.enumerable(),
            "enumerable flag should be preserved"
        );
        assert!(
            !attributes.configurable(),
            "configurable flag should be preserved"
        );
    }

    #[test]
    fn object_heap_traverses_prototype_chain_for_named_properties() {
        let mut heap = ObjectHeap::new();
        let prototype = heap.alloc_object();
        let object = heap.alloc_object();
        let property = PropertyNameId(0);

        heap.set_prototype(object, Some(prototype))
            .expect("prototype link should install");
        heap.set_property(prototype, property, RegisterValue::from_i32(7))
            .expect("prototype property store should succeed");

        let lookup = heap
            .get_property(object, property)
            .expect("prototype lookup should succeed")
            .expect("inherited property should resolve");
        assert_eq!(lookup.owner(), prototype);
        assert_eq!(lookup.cache(), None);
        assert_eq!(
            lookup.value(),
            PropertyValue::data(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn object_heap_supports_strings_and_arrays_fast_paths() {
        let mut heap = ObjectHeap::new();
        let string = heap.alloc_string("otter");
        let array = heap.alloc_array();

        assert_eq!(heap.kind(string), Ok(HeapValueKind::String));
        assert_eq!(heap.kind(array), Ok(HeapValueKind::Array));
        assert_eq!(
            heap.get_builtin_property(string, "length"),
            Ok(Some(RegisterValue::from_i32(5)))
        );

        let character = heap
            .get_index(string, 1)
            .expect("string index should be valid")
            .expect("string index should exist");
        assert!(character.as_object_handle().is_some());

        heap.set_index(array, 0, RegisterValue::from_i32(7))
            .expect("array store should succeed");
        heap.set_index(array, 2, character)
            .expect("array store with hole-fill should succeed");

        assert_eq!(
            heap.get_builtin_property(array, "length"),
            Ok(Some(RegisterValue::from_i32(3)))
        );
        assert_eq!(
            heap.get_index(array, 0),
            Ok(Some(RegisterValue::from_i32(7)))
        );
        assert_eq!(
            heap.set_property(array, PropertyNameId(0), RegisterValue::from_i32(1)),
            Err(ObjectError::InvalidKind)
        );
    }

    #[test]
    fn object_heap_supports_array_define_own_property_semantics() {
        let mut heap = ObjectHeap::new();
        let mut property_names = PropertyNameRegistry::new();
        let array = heap.alloc_array();
        let index_zero = property_names.intern("0");
        let index_one = property_names.intern("1");
        let length = property_names.intern("length");

        assert_eq!(
            heap.define_own_property_from_descriptor_with_registry(
                array,
                index_zero,
                PropertyDescriptor::data(
                    Some(RegisterValue::from_i32(1)),
                    Some(true),
                    Some(true),
                    Some(true),
                ),
                &property_names,
            ),
            Ok(true)
        );
        assert_eq!(heap.array_length(array), Ok(Some(1)));
        assert_eq!(
            heap.get_index(array, 0),
            Ok(Some(RegisterValue::from_i32(1)))
        );

        assert_eq!(
            heap.define_own_property_from_descriptor_with_registry(
                array,
                length,
                PropertyDescriptor::data(Some(RegisterValue::from_i32(0)), None, None, None),
                &property_names,
            ),
            Ok(true)
        );
        assert_eq!(heap.array_length(array), Ok(Some(0)));

        assert_eq!(
            heap.define_own_property_from_descriptor_with_registry(
                array,
                length,
                PropertyDescriptor::data(None, Some(false), None, None),
                &property_names,
            ),
            Ok(true)
        );
        assert_eq!(
            heap.define_own_property_from_descriptor_with_registry(
                array,
                index_one,
                PropertyDescriptor::data(
                    Some(RegisterValue::from_i32(2)),
                    Some(true),
                    Some(true),
                    Some(true),
                ),
                &property_names,
            ),
            Ok(false)
        );
    }

    #[test]
    fn strict_equality_compares_string_contents() {
        let mut heap = ObjectHeap::new();
        let lhs = RegisterValue::from_object_handle(heap.alloc_string("otter").0);
        let rhs = RegisterValue::from_object_handle(heap.alloc_string("otter").0);
        let other = RegisterValue::from_object_handle(heap.alloc_string("vm").0);

        assert_eq!(heap.strict_eq(lhs, rhs), Ok(true));
        assert_eq!(heap.strict_eq(lhs, other), Ok(false));
    }

    #[test]
    fn object_heap_supports_closure_and_upvalue_cells() {
        let mut heap = ObjectHeap::new();
        let upvalue = heap.alloc_upvalue(RegisterValue::from_i32(1));
        let module = crate::module::Module::new(
            Some("closure-module"),
            vec![crate::module::Function::with_bytecode(
                Some("entry"),
                crate::frame::FrameLayout::default(),
                crate::bytecode::Bytecode::default(),
            )],
            FunctionIndex(0),
        )
        .expect("test module should construct");
        let closure = heap.alloc_closure(
            module,
            FunctionIndex(7),
            vec![upvalue],
            ClosureFlags::normal(),
            0,
        );

        assert_eq!(heap.kind(closure), Ok(HeapValueKind::Closure));
        assert_eq!(heap.kind(upvalue), Ok(HeapValueKind::UpvalueCell));
        assert_eq!(heap.closure_callee(closure), Ok(FunctionIndex(7)));
        assert_eq!(heap.closure_upvalue(closure, 0), Ok(upvalue));
        assert_eq!(heap.get_upvalue(upvalue), Ok(RegisterValue::from_i32(1)));

        heap.set_upvalue(upvalue, RegisterValue::from_i32(5))
            .expect("upvalue write should succeed");

        assert_eq!(heap.get_upvalue(upvalue), Ok(RegisterValue::from_i32(5)));
        assert_eq!(
            heap.closure_upvalue(closure, 1),
            Err(ObjectError::InvalidIndex)
        );
    }

    #[test]
    fn object_heap_supports_host_function_objects() {
        let mut heap = ObjectHeap::new();
        let function = heap.alloc_host_function(HostFunctionId(7), 0);
        let property = PropertyNameId(0);

        assert_eq!(heap.kind(function), Ok(HeapValueKind::HostFunction));
        assert_eq!(heap.host_function(function), Ok(Some(HostFunctionId(7))));
        heap.set_property(function, property, RegisterValue::from_i32(9))
            .expect("host function property store should succeed");
        assert_eq!(
            heap.get_property(function, property)
                .expect("host function property lookup should succeed")
                .map(|entry| entry.value()),
            Some(PropertyValue::data(RegisterValue::from_i32(9)))
        );
    }

    #[test]
    fn object_heap_supports_native_objects_with_payload_links() {
        let mut heap = ObjectHeap::new();
        let payload = NativePayloadId(3);
        let prototype = heap.alloc_object();
        let object = heap.alloc_native_object(payload);
        let property = PropertyNameId(0);

        heap.set_prototype(object, Some(prototype))
            .expect("prototype link should install");
        heap.set_property(object, property, RegisterValue::from_i32(11))
            .expect("native object property store should succeed");

        assert_eq!(heap.kind(object), Ok(HeapValueKind::Object));
        assert_eq!(heap.native_payload_id(object), Ok(Some(payload)));
        assert_eq!(
            heap.get_property(object, property)
                .expect("native object lookup should succeed")
                .map(|entry| entry.value()),
            Some(PropertyValue::data(RegisterValue::from_i32(11)))
        );

        let mut seen = Vec::new();
        heap.trace_native_payload_links(&mut |handle, payload_id| seen.push((handle, payload_id)));
        assert_eq!(seen, vec![(object, payload)]);
    }

    #[test]
    fn object_heap_supports_internal_iterators() {
        let mut heap = ObjectHeap::new();
        let array = heap.alloc_array();
        let text = heap.alloc_string("a𐐨");

        heap.set_index(array, 0, RegisterValue::from_i32(7))
            .expect("array store should succeed");
        heap.set_index(array, 1, RegisterValue::from_i32(9))
            .expect("array store should succeed");

        let array_iterator = heap
            .alloc_iterator(array)
            .expect("array iterator should allocate");
        assert_eq!(heap.kind(array_iterator), Ok(HeapValueKind::Iterator));
        assert_eq!(
            heap.iterator_next(array_iterator),
            Ok(IteratorStep::yield_value(RegisterValue::from_i32(7)))
        );
        heap.iterator_close(array_iterator)
            .expect("iterator close should succeed");
        assert_eq!(heap.iterator_next(array_iterator), Ok(IteratorStep::done()));

        let string_iterator = heap
            .alloc_iterator(text)
            .expect("string iterator should allocate");
        let first = heap
            .iterator_next(string_iterator)
            .expect("string iterator should yield")
            .value();
        let second = heap
            .iterator_next(string_iterator)
            .expect("string iterator should yield")
            .value();
        let ascii = RegisterValue::from_object_handle(heap.alloc_string("a").0);
        let astral = RegisterValue::from_object_handle(heap.alloc_string("𐐨").0);

        assert_eq!(heap.strict_eq(first, ascii), Ok(true));
        assert_eq!(heap.strict_eq(second, astral), Ok(true));
        assert_eq!(
            heap.iterator_next(string_iterator),
            Ok(IteratorStep::done())
        );
    }
}
