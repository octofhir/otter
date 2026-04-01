//! Minimal object heap and inline-cache support for the new VM.

use std::collections::BTreeMap;

use otter_gc::typed::{Handle as GcHandle, Traceable, TypedHeap};

use crate::host::HostFunctionId;
use crate::module::Module;
use crate::module::FunctionIndex;
use crate::payload::NativePayloadId;
use crate::property::{PropertyNameId, PropertyNameRegistry};
use crate::value::RegisterValue;

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

/// Error produced by the minimal object heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectError {
    /// The object handle does not exist in the current heap.
    InvalidHandle,
    /// The heap value exists, but the requested operation is not supported.
    InvalidKind,
    /// The heap value exists, but the requested slot index is out of bounds.
    InvalidIndex,
    /// The requested array length is not a valid ECMAScript array length.
    InvalidArrayLength,
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
        value: Box<str>,
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
    },
    HostFunction {
        function: HostFunctionId,
        prototype: Option<ObjectHandle>,
        extensible: bool,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
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
    },
    UpvalueCell {
        value: RegisterValue,
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

impl Traceable for HeapValue {
    fn trace_handles(&self, visitor: &mut dyn FnMut(GcHandle)) {
        match self {
            HeapValue::Object {
                prototype, values, ..
            }
            | HeapValue::NativeObject {
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
            HeapValue::Promise { promise } => {
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
            HeapValue::WeakMap { prototype, .. } | HeapValue::WeakSet { prototype, .. } => {
                if let Some(p) = prototype {
                    trace_handle(*p, visitor);
                }
                // Entries are intentionally NOT traced — they are weak references.
                // Ephemeron fixpoint in collect_with_ephemerons handles value liveness.
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
    /// Creates an empty object heap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            heap: TypedHeap::new(),
            next_shape_id: 1,
        }
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

    /// Map.prototype.get — returns the value for the key, or undefined.
    pub fn map_get(
        &self,
        handle: ObjectHandle,
        key: RegisterValue,
    ) -> Result<RegisterValue, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => {
                for entry in entries.iter().flatten() {
                    if svz(&self.heap,entry.0, key) {
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
                Ok(entries.iter().flatten().any(|e| svz(&self.heap,e.0, key)))
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
            HeapValue::Map { entries, .. } => {
                Ok(entries.iter().filter_map(|e| *e).collect())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Set.prototype.has — returns true if the value exists.
    pub fn set_has(&self, handle: ObjectHandle, value: RegisterValue) -> Result<bool, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => {
                Ok(entries.iter().flatten().any(|e| svz(&self.heap,*e, value)))
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
    pub fn set_values(
        &self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => {
                Ok(entries.iter().filter_map(|e| *e).collect())
            }
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

    /// Allocates a string value.
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::String {
            prototype: None,
            value: value.into(),
        });
        ObjectHandle(h.0)
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
        });
        ObjectHandle(h.0)
    }

    /// Allocates a host-callable native function object.
    pub fn alloc_host_function(&mut self, function: HostFunctionId) -> ObjectHandle {
        let shape_id = self.allocate_shape();
        let h = self.heap.alloc(HeapValue::HostFunction {
            function,
            prototype: None,
            extensible: true,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
        });
        ObjectHandle(h.0)
    }

    /// Allocates a new pending promise.
    pub fn alloc_promise(&mut self) -> ObjectHandle {
        let h = self.heap.alloc(HeapValue::Promise {
            promise: crate::promise::JsPromise::new(),
        });
        ObjectHandle(h.0)
    }

    /// Reads a reference to a JsPromise stored in the heap.
    pub fn get_promise(&self, handle: ObjectHandle) -> Option<&crate::promise::JsPromise> {
        match self.object(handle).ok()? {
            HeapValue::Promise { promise } => Some(promise),
            _ => None,
        }
    }

    /// Reads a mutable reference to a JsPromise.
    pub fn get_promise_mut(
        &mut self,
        handle: ObjectHandle,
    ) -> Option<&mut crate::promise::JsPromise> {
        match self.object_mut(handle).ok()? {
            HeapValue::Promise { promise } => Some(promise),
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
            HeapValue::Map { .. } => Ok(HeapValueKind::Map),
            HeapValue::Set { .. } => Ok(HeapValueKind::Set),
            HeapValue::MapIterator { .. } => Ok(HeapValueKind::MapIterator),
            HeapValue::SetIterator { .. } => Ok(HeapValueKind::SetIterator),
            HeapValue::WeakMap { .. } => Ok(HeapValueKind::WeakMap),
            HeapValue::WeakSet { .. } => Ok(HeapValueKind::WeakSet),
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
            | HeapValue::WeakSet { prototype, .. } => Ok(*prototype),
            HeapValue::UpvalueCell { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. } => Err(ObjectError::InvalidKind),
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
            } => {
                *slot = prototype;
                Ok(true)
            }
            HeapValue::UpvalueCell { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. } => Err(ObjectError::InvalidKind),
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
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Ok(None),
            HeapValue::Closure { .. } => Ok(None),
            HeapValue::Array { elements, .. } if property_name == "length" => Ok(Some(
                RegisterValue::from_i32(i32::try_from(elements.len()).unwrap_or(i32::MAX)),
            )),
            HeapValue::String { value, .. } if property_name == "length" => Ok(Some(
                RegisterValue::from_i32(i32::try_from(value.chars().count()).unwrap_or(i32::MAX)),
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
                let character = value
                    .chars()
                    .nth(index)
                    .map(|ch| ch.to_string().into_boxed_str());
                match character {
                    Some(character) => {
                        let handle = self.alloc_string(character);
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
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Stores an indexed element on a dense array.
    pub fn set_index(
        &mut self,
        handle: ObjectHandle,
        index: usize,
        value: RegisterValue,
    ) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::Array {
                extensible,
                elements,
                indexed_properties,
                elements_writable,
                length_writable,
                ..
            } => {
                if index >= elements.len() {
                    if !*extensible || !*length_writable {
                        return Ok(());
                    }
                    elements.resize(index.saturating_add(1), RegisterValue::hole());
                }

                if let Some(property) = indexed_properties.get_mut(&index) {
                    match property {
                        PropertyValue::Data {
                            value: slot,
                            attributes,
                        } => {
                            if !attributes.writable() {
                                return Ok(());
                            }
                            *slot = value;
                            elements[index] = value;
                            return Ok(());
                        }
                        PropertyValue::Accessor { .. } => return Ok(()),
                    }
                }

                if !*elements_writable {
                    return Ok(());
                }
                elements[index] = value;
                Ok(())
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
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Err(ObjectError::InvalidKind),
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
                        return Ok(false);
                    }

                    let first_non_configurable =
                        indexed_properties
                            .range(length..)
                            .rev()
                            .find_map(|(&index, property)| {
                                (!property.attributes().configurable()).then_some(index)
                            });

                    if let Some(index) = first_non_configurable {
                        elements.truncate(index.saturating_add(1));
                        indexed_properties.retain(|&key, _| key <= index);
                        return Ok(false);
                    }

                    elements.truncate(length);
                    indexed_properties.retain(|&key, _| key < length);
                    return Ok(true);
                }
                if length > elements.len() {
                    if !*extensible || !*length_writable {
                        return Ok(false);
                    }
                    elements.resize(length, RegisterValue::hole());
                }
                Ok(true)
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
            IteratorKind::String => match self.get_index(iterable, next_index)? {
                Some(value) => IteratorStep::yield_value(value),
                None => IteratorStep::done(),
            },
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
                    *next_index = next_index.wrapping_add(1);
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

    pub fn alloc_property_iterator(
        &mut self,
        object: ObjectHandle,
        property_names: &PropertyNameRegistry,
    ) -> Result<ObjectHandle, ObjectError> {
        let mut name_ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut current = Some(object);
        while let Some(h) = current {
            let (obj_keys, proto) = match self.object(h)? {
                HeapValue::Object {
                    keys, prototype, ..
                }
                | HeapValue::NativeObject {
                    keys, prototype, ..
                }
                | HeapValue::Closure {
                    keys, prototype, ..
                }
                | HeapValue::HostFunction {
                    keys, prototype, ..
                } => (keys.clone(), *prototype),
                HeapValue::Array {
                    keys, prototype, ..
                } => (keys.clone(), *prototype),
                _ => (Vec::new(), None),
            };
            for key in obj_keys {
                if seen.insert(key) {
                    name_ids.push(key);
                }
            }
            current = proto;
        }

        // Pre-allocate string handles for all collected keys.
        let key_handles: Vec<ObjectHandle> = name_ids
            .iter()
            .filter(|id| !property_names.is_symbol(**id))
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

    /// Returns the owning module stored in a closure object.
    pub fn closure_module(&self, handle: ObjectHandle) -> Result<Module, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { module, .. } => Ok(module.clone()),
            _ => Err(ObjectError::InvalidKind),
        }
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
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Ok(None),
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
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Ok(None),
        }
    }

    /// Returns the borrowed string contents for string heap values.
    pub fn string_value(&self, handle: ObjectHandle) -> Result<Option<&str>, ObjectError> {
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
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Ok(None),
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
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Ok(None),
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
            | HeapValue::BoundFunction { .. } => {
                self.set_named_property_storage(handle, property, value)
            }
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
            | HeapValue::BoundFunction { .. } => self.delete_ordinary_property(handle, property),
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
            | HeapValue::BoundFunction { .. } => self.delete_ordinary_property(handle, property),
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
            | HeapValue::BoundFunction { .. } => {
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
            | HeapValue::BoundFunction { .. } => {
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

        match next {
            PropertyValue::Data { value, attributes } => {
                if index >= elements.len() {
                    if !*extensible || !*length_writable {
                        return Ok(false);
                    }
                    elements.resize(index.saturating_add(1), RegisterValue::hole());
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
                Ok(true)
            }
            PropertyValue::Accessor {
                getter,
                setter,
                attributes,
            } => {
                if index >= elements.len() {
                    if !*extensible || !*length_writable {
                        return Ok(false);
                    }
                    elements.resize(index.saturating_add(1), RegisterValue::hole());
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
                Ok(true)
            }
        }
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
            HeapValue::Array { .. }
            | HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => return Err(ObjectError::InvalidKind),
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
            | HeapValue::BoundFunction { keys, .. } => Ok(keys.clone()),
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
            | HeapValue::BoundFunction { keys, .. } => Ok(keys.clone()),
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
                let length = value.chars().count();
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
            | HeapValue::Array { keys, .. } => Ok(property_slot(keys, property).is_some()),
            _ => Ok(false),
        }
    }

    /// ES2024 §10.4.1.3 — Allocates a bound function exotic object.
    pub fn alloc_bound_function(
        &mut self,
        target: ObjectHandle,
        bound_this: RegisterValue,
        bound_args: Vec<RegisterValue>,
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
            Ok(HeapValueKind::HostFunction | HeapValueKind::Closure | HeapValueKind::BoundFunction)
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
            | HeapValue::BoundFunction { extensible, .. } => Ok(*extensible),
            // Iterator objects are extensible (prototype must be settable during init).
            HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            // WeakMap/WeakSet are extensible (prototype must be settable during init).
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => Ok(true),
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
            | HeapValue::BoundFunction { extensible, .. } => {
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
            | HeapValue::BoundFunction { values, .. } => values,
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
            | HeapValue::BoundFunction { values, .. } => values,
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
            | HeapValue::BoundFunction { keys, values, .. } => (keys, values),
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
            | HeapValue::BoundFunction { keys, values, .. } => (keys, values),
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
            | HeapValue::Array { keys, .. } => property_slot(keys, property),
            HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => {
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
                | HeapValue::Array { shape_id, .. } => *shape_id,
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
                    | HeapValue::BoundFunction { values, .. } => values,
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
                | HeapValue::BoundFunction { values, .. } => values,
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
            | HeapValue::BoundFunction { keys, .. } => property_slot(keys, property),
            HeapValue::Array { keys, .. } if include_array => property_slot(keys, property),
            HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. }
            | HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => return Err(ObjectError::InvalidKind),
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
                            if self.heap.is_marked(GcHandle(key)) {
                                if let Some(vh) = value.as_object_handle() {
                                    if !self.heap.is_marked(GcHandle(vh)) {
                                        extra.push(GcHandle(vh));
                                    }
                                }
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
                            RegisterValue::from_i32(
                                i32::try_from(value.chars().count()).unwrap_or(i32::MAX),
                            ),
                            PropertyAttributes::from_flags(false, false, false),
                        ),
                        PropertyInlineCache::new(ObjectShapeId(0), 0),
                    )));
                }
                return Ok(None);
            }
            HeapValue::UpvalueCell { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. } => return Err(ObjectError::InvalidKind),
            // Iterators have no own properties but participate in prototype chain.
            HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. }
            | HeapValue::MapIterator { .. }
            | HeapValue::SetIterator { .. } => return Ok(None),
            HeapValue::Map { .. }
            | HeapValue::Set { .. }
            | HeapValue::WeakMap { .. }
            | HeapValue::WeakSet { .. } => return Ok(None),
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
            | HeapValue::WeakSet { prototype, .. } => Ok(*prototype),
            HeapValue::UpvalueCell { .. }
            | HeapValue::PropertyIterator { .. }
            | HeapValue::Promise { .. } => Err(ObjectError::InvalidKind),
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
        let closure =
            heap.alloc_closure(module, FunctionIndex(7), vec![upvalue], ClosureFlags::normal());

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
        let function = heap.alloc_host_function(HostFunctionId(7));
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
    if let (Some(ah), Some(bh)) = (a.as_object_handle(), b.as_object_handle()) {
        if let (Some(hva), Some(hvb)) = (
            heap.get::<HeapValue>(GcHandle(ah)),
            heap.get::<HeapValue>(GcHandle(bh)),
        ) {
            if let (HeapValue::String { value: sa, .. }, HeapValue::String { value: sb, .. }) = (hva, hvb) {
                return sa == sb;
            }
        }
    }
    false
}

impl ObjectHeap {
    /// Find the index of a matching Map entry by key using SameValueZero.
    fn map_find_index(&self, handle: ObjectHandle, key: RegisterValue) -> Result<Option<usize>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Map { entries, .. } => {
                for (i, entry) in entries.iter().enumerate() {
                    if let Some(e) = entry {
                        if svz(&self.heap, e.0, key) {
                            return Ok(Some(i));
                        }
                    }
                }
                Ok(None)
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Find the index of a matching Set entry by value using SameValueZero.
    fn set_find_index(&self, handle: ObjectHandle, value: RegisterValue) -> Result<Option<usize>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Set { entries, .. } => {
                for (i, entry) in entries.iter().enumerate() {
                    if let Some(e) = entry {
                        if svz(&self.heap, *e, value) {
                            return Ok(Some(i));
                        }
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
    if let Some(n) = v.as_number() {
        if n == 0.0 && n.is_sign_negative() {
            return RegisterValue::from_number(0.0);
        }
    }
    v
}
