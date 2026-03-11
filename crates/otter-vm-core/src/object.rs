//! JavaScript objects with hidden classes (shapes)
//!
//! Objects use hidden classes (called "shapes") for property access optimization.
//!
//! ## Inline Properties
//!
//! The first few properties (up to `INLINE_PROPERTY_COUNT`) are stored inline
//! in the object struct rather than in a separate Vec. This improves cache
//! locality and reduces indirection for common cases where objects have few
//! properties.

use crate::memory::MemoryManager;
use crate::object_cell::ObjectCell;
use indexmap::IndexMap;
use std::sync::Arc;

use crate::gc::GcRef;
use crate::shape::Shape;
use crate::value::UpvalueCell;
use otter_vm_gc::object::MarkColor;

use std::sync::atomic::{AtomicU64, Ordering};

/// Global prototype epoch counter for IC invalidation.
/// Incremented whenever any object's prototype is modified.
/// Used by inline caches to detect when prototype chain lookups may be stale.
static PROTO_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Get the current global prototype epoch.
/// Used by IC code to record the epoch when caching prototype chain lookups.
#[inline]
pub fn get_proto_epoch() -> u64 {
    PROTO_EPOCH.load(Ordering::Acquire)
}

/// Increment the global prototype epoch.
/// Called whenever an object's prototype is modified.
/// Returns the new epoch value.
#[inline]
pub fn bump_proto_epoch() -> u64 {
    PROTO_EPOCH.fetch_add(1, Ordering::AcqRel) + 1
}

/// Maximum prototype chain depth to prevent stack overflow
const MAX_PROTOTYPE_CHAIN_DEPTH: usize = 100;

/// Number of properties stored inline in the object
/// Properties beyond this count overflow to a Vec.
pub const INLINE_PROPERTY_COUNT: usize = 8;

/// Threshold for transitioning to dictionary mode.
/// Objects with more than this many properties switch to HashMap-based storage
/// for better memory efficiency at the cost of IC cacheability.
pub const DICTIONARY_THRESHOLD: usize = 64;

/// Number of property deletions before forcing dictionary mode transition.
/// Slot-clearing keeps IC working for 1-2 deletes; beyond that, dictionary
/// mode avoids accumulating too many dead slots in the shape chain.
const DELETE_DICTIONARY_THRESHOLD: u8 = 3;
use crate::string::JsString;
use crate::value::Value;

/// Combined write barrier for incremental + generational GC.
///
/// **Incremental** (Dijkstra): during marking phase, ensures stored values are
/// grayed so the incremental marker visits them.
///
/// **Generational**: if the stored value is in the nursery (young generation),
/// adds it to the remembered set so the young-only minor GC can find it.
///
/// Fast path for non-heap values: `gc_header()` returns None → immediate return.
/// Fast path for old-gen heap values: nursery range check (2 comparisons) → false.
#[inline]
pub fn gc_write_barrier(value: &Value) {
    // Fast exit for non-heap values (numbers, booleans, undefined, null)
    let header_ptr = match value.gc_header() {
        Some(ptr) => ptr,
        None => return,
    };

    // Generational barrier: if value is in the nursery, add to remembered set.
    otter_vm_gc::remembered_set_add_if_young(header_ptr);

    // Incremental barrier: only active during marking phase.
    let registry = otter_vm_gc::global_registry();
    if registry.is_marking() {
        gc_write_barrier_incremental(header_ptr);
    }
}

/// Incremental GC barrier slow path — gray the object and push to worklist.
#[inline(never)]
fn gc_write_barrier_incremental(header_ptr: *const otter_vm_gc::GcHeader) {
    let header = unsafe { &*header_ptr };
    if header.mark() == MarkColor::White {
        header.set_mark(MarkColor::Gray);
        otter_vm_gc::barrier_push(header_ptr);
    }
}

/// Write barrier for a PropertyDescriptor being stored (used in dictionary mode).
#[inline]
fn gc_write_barrier_desc(desc: &PropertyDescriptor) {
    match desc {
        PropertyDescriptor::Data { value, .. } => gc_write_barrier(value),
        PropertyDescriptor::Accessor { get, set, .. } => {
            if let Some(g) = get {
                gc_write_barrier(g);
            }
            if let Some(s) = set {
                gc_write_barrier(s);
            }
        }
        PropertyDescriptor::Deleted => {}
    }
}

/// GC-managed accessor pair holding getter and setter values.
/// Stored as a Value in accessor property slots via `Value::accessor_pair()`.
pub struct AccessorPair {
    /// Getter function (or undefined if no getter)
    pub getter: Value,
    /// Setter function (or undefined if no setter)
    pub setter: Value,
}

// SAFETY: AccessorPair is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
unsafe impl Send for AccessorPair {}
unsafe impl Sync for AccessorPair {}

impl otter_vm_gc::GcTraceable for AccessorPair {
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::ACCESSOR_PAIR;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        self.getter.trace(tracer);
        self.setter.trace(tracer);
    }
}

/// Packed 1-byte metadata per property slot.
///
/// Encodes property kind (data/accessor/empty/deleted) and attributes
/// (writable, enumerable, configurable) in a single byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct SlotMeta(u8);

impl SlotMeta {
    // Kind bits (lower 2 bits)
    const KIND_MASK: u8 = 0b0000_0011;
    const KIND_EMPTY: u8 = 0b00;
    const KIND_DATA: u8 = 0b01;
    const KIND_ACCESSOR: u8 = 0b10;

    // Attribute bits
    const WRITABLE_BIT: u8 = 0b0000_0100;
    const ENUMERABLE_BIT: u8 = 0b0000_1000;
    const CONFIGURABLE_BIT: u8 = 0b0001_0000;

    /// Empty slot (no property stored here)
    pub const EMPTY: Self = Self(Self::KIND_EMPTY);

    /// Create metadata for a data property
    #[inline]
    pub fn data(attrs: PropertyAttributes) -> Self {
        let mut bits = Self::KIND_DATA;
        if attrs.writable {
            bits |= Self::WRITABLE_BIT;
        }
        if attrs.enumerable {
            bits |= Self::ENUMERABLE_BIT;
        }
        if attrs.configurable {
            bits |= Self::CONFIGURABLE_BIT;
        }
        Self(bits)
    }

    /// Create metadata for an accessor property
    #[inline]
    pub fn accessor(attrs: PropertyAttributes) -> Self {
        let mut bits = Self::KIND_ACCESSOR;
        if attrs.enumerable {
            bits |= Self::ENUMERABLE_BIT;
        }
        if attrs.configurable {
            bits |= Self::CONFIGURABLE_BIT;
        }
        Self(bits)
    }

    /// Default data property metadata (writable, enumerable, configurable)
    pub const DEFAULT_DATA: Self =
        Self(Self::KIND_DATA | Self::WRITABLE_BIT | Self::ENUMERABLE_BIT | Self::CONFIGURABLE_BIT);

    #[inline]
    pub fn is_empty(self) -> bool {
        self.0 & Self::KIND_MASK == Self::KIND_EMPTY
    }

    #[inline]
    pub fn is_data(self) -> bool {
        self.0 & Self::KIND_MASK == Self::KIND_DATA
    }

    #[inline]
    pub fn is_accessor(self) -> bool {
        self.0 & Self::KIND_MASK == Self::KIND_ACCESSOR
    }

    #[inline]
    pub fn is_writable(self) -> bool {
        self.0 & Self::WRITABLE_BIT != 0
    }

    #[inline]
    pub fn is_enumerable(self) -> bool {
        self.0 & Self::ENUMERABLE_BIT != 0
    }

    #[inline]
    pub fn is_configurable(self) -> bool {
        self.0 & Self::CONFIGURABLE_BIT != 0
    }

    /// Convert to PropertyAttributes
    #[inline]
    pub fn to_attributes(self) -> PropertyAttributes {
        PropertyAttributes {
            writable: self.is_writable(),
            enumerable: self.is_enumerable(),
            configurable: self.is_configurable(),
        }
    }

    /// Update writable bit
    #[inline]
    pub fn with_writable(self, w: bool) -> Self {
        if w {
            Self(self.0 | Self::WRITABLE_BIT)
        } else {
            Self(self.0 & !Self::WRITABLE_BIT)
        }
    }

    /// Update configurable bit
    #[inline]
    pub fn with_configurable(self, c: bool) -> Self {
        if c {
            Self(self.0 | Self::CONFIGURABLE_BIT)
        } else {
            Self(self.0 & !Self::CONFIGURABLE_BIT)
        }
    }

    /// Update enumerable bit
    #[inline]
    pub fn with_enumerable(self, e: bool) -> Self {
        if e {
            Self(self.0 | Self::ENUMERABLE_BIT)
        } else {
            Self(self.0 & !Self::ENUMERABLE_BIT)
        }
    }
}

/// Property key (string or symbol)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PropertyKey {
    /// String property key (GC-managed)
    String(GcRef<JsString>),
    /// Symbol property key (GC-managed)
    Symbol(GcRef<crate::value::Symbol>),
    /// Integer index (for arrays)
    Index(u32),
}

impl PropertyKey {
    /// Ensure any GC-managed components (Strings/Symbols) in this key are tenured.
    ///
    /// This is required when storing keys in non-GC objects like Shapes,
    /// because the GC won't see them as roots unless they are tenured or
    /// the specific Shape is traced (which only happens from live JsObjects).
    pub fn ensure_tenured(&self) {
        match self {
            Self::String(s) => {
                s.header().set_tenured();
                s.ensure_tenured();
            }
            Self::Symbol(sym) => sym.header().set_tenured(),
            Self::Index(_) => {}
        }
    }

    /// Maximum valid array index per ECMA-262: 0 .. 2^32 - 2.
    /// The value 2^32 - 1 (4294967295) is NOT a valid array index.
    pub const MAX_ARRAY_INDEX: u32 = u32::MAX - 1; // 4294967294

    /// Create a string property key (canonicalizes numeric strings to Index)
    pub fn string(s: &str) -> Self {
        // Canonicalize numeric strings to Index for consistent lookup.
        // Only values 0..=MAX_ARRAY_INDEX are valid array indices per spec.
        if let Some(n) = Self::parse_canonical_array_index_bytes(s.as_bytes()) {
            return Self::Index(n);
        }
        let js_str = JsString::intern(s);
        Self::String(js_str)
    }

    /// Create from a GcRef<JsString>
    pub fn from_js_string(s: GcRef<JsString>) -> Self {
        // Canonicalize numeric strings to Index for consistent lookup
        if let Some(n) = Self::parse_canonical_array_index_utf16(s.as_utf16()) {
            return Self::Index(n);
        }
        Self::String(s)
    }

    /// Create an index property key
    pub fn index(i: u32) -> Self {
        Self::Index(i)
    }

    #[inline]
    fn parse_canonical_array_index_bytes(bytes: &[u8]) -> Option<u32> {
        if bytes.is_empty() || (bytes.len() > 1 && bytes[0] == b'0') {
            return None;
        }

        let mut value: u32 = 0;
        for &b in bytes {
            if !b.is_ascii_digit() {
                return None;
            }
            value = value.checked_mul(10)?;
            value = value.checked_add((b - b'0') as u32)?;
        }

        if value <= Self::MAX_ARRAY_INDEX {
            Some(value)
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn parse_canonical_array_index_utf16(units: &[u16]) -> Option<u32> {
        if units.is_empty() || (units.len() > 1 && units[0] == b'0' as u16) {
            return None;
        }

        let mut value: u32 = 0;
        for &unit in units {
            if !(b'0' as u16..=b'9' as u16).contains(&unit) {
                return None;
            }
            value = value.checked_mul(10)?;
            value = value.checked_add((unit - b'0' as u16) as u32)?;
        }

        if value <= Self::MAX_ARRAY_INDEX {
            Some(value)
        } else {
            None
        }
    }

    /// Trace property key for GC
    pub fn trace(&self, tracer: &mut dyn crate::gc::Tracer) {
        match self {
            Self::String(s) => {
                // GcRef provides header() via GcBox wrapper
                tracer.mark_header(s.header() as *const _);
            }
            Self::Symbol(sym) => {
                tracer.mark_header(sym.header() as *const _);
            }
            _ => {}
        }
    }
}

impl From<&str> for PropertyKey {
    fn from(s: &str) -> Self {
        Self::string(s)
    }
}

impl From<u32> for PropertyKey {
    fn from(i: u32) -> Self {
        Self::Index(i)
    }
}

/// Property attributes
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PropertyAttributes {
    /// Property is writable
    pub writable: bool,
    /// Property is enumerable
    pub enumerable: bool,
    /// Property is configurable
    pub configurable: bool,
}

impl PropertyAttributes {
    /// Default data property attributes (all true — use only for user-created properties)
    pub const fn data() -> Self {
        Self {
            writable: true,
            enumerable: true,
            configurable: true,
        }
    }

    /// Default accessor property attributes (enumerable, configurable, no writable)
    pub const fn accessor() -> Self {
        Self {
            writable: false, // Not applicable to accessors, but kept for structural consistency
            enumerable: true,
            configurable: true,
        }
    }

    /// Non-writable, non-enumerable, non-configurable
    pub const fn frozen() -> Self {
        Self {
            writable: false,
            enumerable: false,
            configurable: false,
        }
    }

    /// Builtin method attributes: `{ writable: true, enumerable: false, configurable: true }`
    ///
    /// Per ES2023 §10.4.1, built-in function properties on prototypes are
    /// writable and configurable but NOT enumerable.
    pub const fn builtin_method() -> Self {
        Self {
            writable: true,
            enumerable: false,
            configurable: true,
        }
    }

    /// Function `length` and `name` property attributes:
    /// `{ writable: false, enumerable: false, configurable: true }`
    ///
    /// Per ES2023 §10.2.8, the `length` and `name` properties of built-in
    /// function objects are not writable and not enumerable, but configurable.
    pub const fn function_length() -> Self {
        Self {
            writable: false,
            enumerable: false,
            configurable: true,
        }
    }

    /// Constructor link attributes (same as builtin_method):
    /// `{ writable: true, enumerable: false, configurable: true }`
    ///
    /// Used for the `constructor` property on prototype objects.
    pub const fn constructor_link() -> Self {
        Self {
            writable: true,
            enumerable: false,
            configurable: true,
        }
    }

    /// Permanent constant attributes:
    /// `{ writable: false, enumerable: false, configurable: false }`
    ///
    /// Used for well-known symbols on `Symbol` constructor and similar constants.
    pub const fn permanent() -> Self {
        Self {
            writable: false,
            enumerable: false,
            configurable: false,
        }
    }

    /// Non-enumerable accessor attributes:
    /// `{ enumerable: false, configurable: true }`
    ///
    /// Used for builtin accessors (getters/setters) on prototypes.
    pub const fn builtin_accessor() -> Self {
        Self {
            writable: false, // Not applicable to accessors
            enumerable: false,
            configurable: true,
        }
    }
}

/// Property descriptor
#[derive(Clone, Debug)]
pub enum PropertyDescriptor {
    /// Data property
    Data {
        /// The value
        value: Value,
        /// Attributes
        attributes: PropertyAttributes,
    },
    /// Accessor property
    Accessor {
        /// Getter function
        get: Option<Value>,
        /// Setter function
        set: Option<Value>,
        /// Attributes
        attributes: PropertyAttributes,
    },
    /// Tombstone for a deleted property.
    ///
    /// We keep the key in the object's Shape (hidden class) for now, but treat this
    /// descriptor as "absent" for `get`/`has`/`own_keys` so deletion is observable.
    Deleted,
}

impl PropertyDescriptor {
    /// Create a data property (default: all-true attributes — for user code)
    pub fn data(value: Value) -> Self {
        Self::Data {
            value,
            attributes: PropertyAttributes::data(),
        }
    }

    /// Create a data property with specific attributes
    pub fn data_with_attrs(value: Value, attributes: PropertyAttributes) -> Self {
        Self::Data { value, attributes }
    }

    /// Create a builtin method property (non-enumerable, writable, configurable)
    pub fn builtin_method(value: Value) -> Self {
        Self::Data {
            value,
            attributes: PropertyAttributes::builtin_method(),
        }
    }

    /// Create a builtin data property (non-enumerable, writable, configurable)
    /// Same attributes as builtin_method but semantically for data values.
    pub fn builtin_data(value: Value) -> Self {
        Self::Data {
            value,
            attributes: PropertyAttributes::builtin_method(),
        }
    }

    /// Create a non-writable, non-enumerable, configurable property
    /// (for function `length` and `name` properties)
    pub fn function_length(value: Value) -> Self {
        Self::Data {
            value,
            attributes: PropertyAttributes::function_length(),
        }
    }

    /// Get the value (for data properties)
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::Data { value, .. } => Some(value),
            Self::Accessor { .. } | Self::Deleted => None,
        }
    }

    /// Get value mutably
    pub fn value_mut(&mut self) -> Option<&mut Value> {
        match self {
            Self::Data { value, .. } => Some(value),
            Self::Accessor { .. } | Self::Deleted => None,
        }
    }

    /// Check if writable
    pub fn is_writable(&self) -> bool {
        match self {
            Self::Data { attributes, .. } => attributes.writable,
            Self::Accessor { .. } | Self::Deleted => false,
        }
    }

    /// Check if configurable
    pub fn is_configurable(&self) -> bool {
        match self {
            Self::Data { attributes, .. } | Self::Accessor { attributes, .. } => {
                attributes.configurable
            }
            Self::Deleted => true,
        }
    }

    /// Check if enumerable
    pub fn enumerable(&self) -> bool {
        match self {
            Self::Data { attributes, .. } | Self::Accessor { attributes, .. } => {
                attributes.enumerable
            }
            Self::Deleted => false,
        }
    }
    /// Create an accessor property with just a getter
    pub fn getter(get: Value) -> Self {
        Self::Accessor {
            get: Some(get),
            set: None,
            attributes: PropertyAttributes::builtin_accessor(),
        }
    }
}

// PropertyEntry removed — properties are now stored as flat Value slots + SlotMeta.

/// Error returned when a property assignment fails.
///
/// In strict mode, these map to specific `TypeError` messages per the spec.
/// In sloppy mode, the assignment silently fails.
#[derive(Debug)]
pub enum SetPropertyError {
    /// Object is frozen — no properties can be changed or added.
    Frozen,
    /// Property exists but is non-writable.
    NonWritable,
    /// Object is not extensible — new properties cannot be added.
    NonExtensible,
    /// Object is sealed — new properties cannot be added and existing
    /// properties cannot be reconfigured.
    Sealed,
    /// Property is an accessor without a setter.
    AccessorWithoutSetter,
    /// Array length assignment was invalid and must throw `RangeError`.
    InvalidArrayLength,
}

impl std::fmt::Display for SetPropertyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frozen => write!(f, "Cannot assign to property of a frozen object"),
            Self::NonWritable => write!(f, "Cannot assign to read only property"),
            Self::NonExtensible => write!(f, "Cannot add property, object is not extensible"),
            Self::Sealed => write!(f, "Cannot add property to a sealed object"),
            Self::AccessorWithoutSetter => write!(f, "Cannot set property which has only a getter"),
            Self::InvalidArrayLength => write!(f, "Invalid array length"),
        }
    }
}

/// A property descriptor from ToPropertyDescriptor (ES2026 §6.2.5.5)
/// where fields can be absent. Used as input to [[DefineOwnProperty]].
///
/// Unlike `PropertyDescriptor` (which is always fully specified for storage),
/// this type distinguishes "absent" (None) from "present" for each field.
#[derive(Clone, Debug)]
pub struct PartialDescriptor {
    /// [[Value]] — present only if this is (or should become) a data descriptor
    pub value: Option<Value>,
    /// [[Writable]] — present only if this is (or should become) a data descriptor
    pub writable: Option<bool>,
    /// [[Get]] — present only if this is (or should become) an accessor descriptor.
    /// `Some(Value::undefined())` means explicitly "no getter".
    pub get: Option<Value>,
    /// [[Set]] — present only if this is (or should become) an accessor descriptor.
    /// `Some(Value::undefined())` means explicitly "no setter".
    pub set: Option<Value>,
    /// [[Enumerable]]
    pub enumerable: Option<bool>,
    /// [[Configurable]]
    pub configurable: Option<bool>,
}

impl PartialDescriptor {
    /// ES2026 §6.2.5.1 IsAccessorDescriptor
    pub fn is_accessor_descriptor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }

    /// ES2026 §6.2.5.2 IsDataDescriptor
    pub fn is_data_descriptor(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }

    /// ES2026 §6.2.5.3 IsGenericDescriptor
    pub fn is_generic_descriptor(&self) -> bool {
        !self.is_accessor_descriptor() && !self.is_data_descriptor()
    }

    /// Check if all fields are absent (empty descriptor)
    pub fn is_empty(&self) -> bool {
        self.value.is_none()
            && self.writable.is_none()
            && self.get.is_none()
            && self.set.is_none()
            && self.enumerable.is_none()
            && self.configurable.is_none()
    }

    /// Check if this data descriptor has any non-default attributes.
    /// Default data property attributes are: writable=true, enumerable=true, configurable=true.
    pub fn has_non_default_data_attributes(&self) -> bool {
        matches!(self.writable, Some(false))
            || matches!(self.enumerable, Some(false))
            || matches!(self.configurable, Some(false))
    }

    /// Create from a fully-specified PropertyDescriptor (all fields present).
    pub fn from_full(desc: &PropertyDescriptor) -> Self {
        match desc {
            PropertyDescriptor::Data { value, attributes } => PartialDescriptor {
                value: Some(value.clone()),
                writable: Some(attributes.writable),
                get: None,
                set: None,
                enumerable: Some(attributes.enumerable),
                configurable: Some(attributes.configurable),
            },
            PropertyDescriptor::Accessor {
                get,
                set,
                attributes,
            } => PartialDescriptor {
                value: None,
                writable: None,
                get: Some(get.clone().unwrap_or(Value::undefined())),
                set: Some(set.clone().unwrap_or(Value::undefined())),
                enumerable: Some(attributes.enumerable),
                configurable: Some(attributes.configurable),
            },
            PropertyDescriptor::Deleted => PartialDescriptor {
                value: None,
                writable: None,
                get: None,
                set: None,
                enumerable: None,
                configurable: None,
            },
        }
    }
}

/// Get a property value from an object, properly invoking accessor getters.
///
/// Unlike `JsObject::get()` which returns `None` for accessor properties,
/// this function invokes the getter function when the property is an accessor.
pub fn get_value_full(
    obj: &crate::gc::GcRef<JsObject>,
    key: &PropertyKey,
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, crate::error::VmError> {
    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if !getter.is_undefined() {
                        let this_val = Value::object(obj.clone());
                        return ncx.call_function(&getter, this_val, &[]);
                    }
                }
                Ok(Value::undefined())
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Spec-compliant Set(O, P, V) that invokes JS setter for accessor properties.
/// Unlike `JsObject::set()` which returns an error for accessors, this calls the setter.
pub(crate) fn set_value_full(
    obj: &crate::gc::GcRef<JsObject>,
    key: &PropertyKey,
    value: Value,
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<(), crate::error::VmError> {
    // Check if the property is an accessor (own or inherited)
    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Accessor { set, .. } => {
                if let Some(setter) = set {
                    if !setter.is_undefined() {
                        let this_val = Value::object(obj.clone());
                        ncx.call_function(&setter, this_val, &[value])?;
                        return Ok(());
                    }
                }
                return Err(crate::error::VmError::type_error(
                    "Cannot set property which has only a getter",
                ));
            }
            PropertyDescriptor::Data { attributes, .. } => {
                if !attributes.writable {
                    return Err(crate::error::VmError::type_error(
                        "Cannot assign to read only property",
                    ));
                }
                let _ = obj.set(key.clone(), value);
                return Ok(());
            }
            PropertyDescriptor::Deleted => {}
        }
    }
    // Property doesn't exist or was deleted — add it
    let _ = obj.set(key.clone(), value);
    Ok(())
}

/// Parse a JS object into a PartialDescriptor (ES2026 §6.2.5.5 ToPropertyDescriptor).
///
/// Uses `.has()` to distinguish absent fields from present-but-undefined fields.
/// Validates accessor callability: get/set must be callable or undefined.
/// Takes NativeContext to properly invoke accessor getters on the descriptor object.
pub fn to_property_descriptor(
    attr_obj: &crate::gc::GcRef<JsObject>,
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<PartialDescriptor, String> {
    let has_value = attr_obj.has(&PropertyKey::from("value"));
    let has_writable = attr_obj.has(&PropertyKey::from("writable"));
    let has_get = attr_obj.has(&PropertyKey::from("get"));
    let has_set = attr_obj.has(&PropertyKey::from("set"));
    let has_enumerable = attr_obj.has(&PropertyKey::from("enumerable"));
    let has_configurable = attr_obj.has(&PropertyKey::from("configurable"));

    // Step 3-4: Check for conflicting data + accessor fields
    if (has_value || has_writable) && (has_get || has_set) {
        return Err(
            "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute"
                .to_string(),
        );
    }

    let enumerable = if has_enumerable {
        let v = get_value_full(attr_obj, &PropertyKey::from("enumerable"), ncx)
            .map_err(|e| e.to_string())?;
        Some(v.to_boolean())
    } else {
        None
    };

    let configurable = if has_configurable {
        let v = get_value_full(attr_obj, &PropertyKey::from("configurable"), ncx)
            .map_err(|e| e.to_string())?;
        Some(v.to_boolean())
    } else {
        None
    };

    let value = if has_value {
        Some(
            get_value_full(attr_obj, &PropertyKey::from("value"), ncx)
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    let writable = if has_writable {
        let v = get_value_full(attr_obj, &PropertyKey::from("writable"), ncx)
            .map_err(|e| e.to_string())?;
        Some(v.to_boolean())
    } else {
        None
    };

    let get = if has_get {
        let g =
            get_value_full(attr_obj, &PropertyKey::from("get"), ncx).map_err(|e| e.to_string())?;
        // Step 7.b: If getter is not callable and not undefined, throw TypeError
        if !g.is_undefined() && !g.is_callable() {
            return Err("Getter must be a function".to_string());
        }
        Some(g)
    } else {
        None
    };

    let set = if has_set {
        let s =
            get_value_full(attr_obj, &PropertyKey::from("set"), ncx).map_err(|e| e.to_string())?;
        // Step 8.b: If setter is not callable and not undefined, throw TypeError
        if !s.is_undefined() && !s.is_callable() {
            return Err("Setter must be a function".to_string());
        }
        Some(s)
    } else {
        None
    };

    Ok(PartialDescriptor {
        value,
        writable,
        get,
        set,
        enumerable,
        configurable,
    })
}

/// Parameter mapping for mapped arguments objects (ES2024 §10.4.4).
/// Each cell aliases a formal parameter's local variable slot via UpvalueCell.
pub struct ArgumentMapping {
    /// One entry per formal parameter. Some(cell) = aliased, None = unmapped.
    pub cells: Vec<Option<UpvalueCell>>,
}

/// A JavaScript object
///
/// Thread-confined with zero-cost interior mutability via `ObjectCell`.
///
/// ## Inline Properties
///
/// The first `INLINE_PROPERTY_COUNT` properties are stored inline in the object
/// for faster access. Additional properties overflow to the `properties` Vec.
/// Both inline and overflow use flat Value slots + SlotMeta for storage.

/// The internal storage kind for indexed properties (elements).
/// Used to optimize array storage and operations for common types (SMI and Doubles).
#[derive(Clone, Debug)]
pub enum ElementsKind {
    /// Dense storage for Small Integers (32-bit signed)
    Smi(Vec<i32>),
    /// Dense storage for 64-bit IEEE-754 numbers
    Double(Vec<f64>),
    /// General storage for any Value (including holes)
    Object(Vec<Value>),
}

impl ElementsKind {
    /// Create a new empty Object elements store
    pub fn new() -> Self {
        ElementsKind::Object(Vec::new())
    }

    /// Return the length of the elements vector
    pub fn len(&self) -> usize {
        match self {
            ElementsKind::Smi(v) => v.len(),
            ElementsKind::Double(v) => v.len(),
            ElementsKind::Object(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }



    /// Clear the elements
    pub fn clear(&mut self) {
        match self {
            ElementsKind::Smi(v) => v.clear(),
            ElementsKind::Double(v) => v.clear(),
            ElementsKind::Object(v) => v.clear(),
        }
    }

    pub fn get(&self, index: usize) -> Option<Value> {
        match self {
            ElementsKind::Smi(v) => v.get(index).map(|&x| Value::int32(x)),
            ElementsKind::Double(v) => v.get(index).map(|&x| Value::number(x)),
            ElementsKind::Object(v) => v.get(index).cloned(),
        }
    }

    pub fn set(&mut self, index: usize, value: Value) {
        if index >= self.len() {
            // Should not happen if correctly resized, but fallback to object transition to be safe
            self.transition_to_object();
            if let ElementsKind::Object(v) = self {
                if index >= v.len() {
                    v.resize(index + 1, Value::hole());
                }
                v[index] = value;
            }
            return;
        }

        match self {
            ElementsKind::Smi(v) => {
                if value.is_int32() {
                    v[index] = value.as_int32().unwrap();
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2[index] = value;
                    }
                }
            }
            ElementsKind::Double(v) => {
                if value.is_number() {
                    v[index] = value.as_number().unwrap();
                } else if value.is_int32() {
                    v[index] = value.as_int32().unwrap() as f64;
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2[index] = value;
                    }
                }
            }
            ElementsKind::Object(v) => {
                v[index] = value;
            }
        }
    }

    pub fn push(&mut self, value: Value) {
        match self {
            ElementsKind::Smi(v) => {
                if value.is_int32() {
                    v.push(value.as_int32().unwrap());
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2.push(value);
                    }
                }
            }
            ElementsKind::Double(v) => {
                if value.is_number() {
                    v.push(value.as_number().unwrap());
                } else if value.is_int32() {
                    v.push(value.as_int32().unwrap() as f64);
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2.push(value);
                    }
                }
            }
            ElementsKind::Object(v) => {
                v.push(value);
            }
        }
    }

    pub fn pop(&mut self) -> Option<Value> {
        match self {
            ElementsKind::Smi(v) => v.pop().map(Value::int32),
            ElementsKind::Double(v) => v.pop().map(Value::number),
            ElementsKind::Object(v) => v.pop(),
        }
    }

    pub fn resize(&mut self, new_len: usize, filler: Value) {
        if new_len > self.len() && (!filler.is_int32() && !filler.is_number()) {
            self.transition_to_object();
        }

        match self {
            ElementsKind::Smi(v) => {
                if filler.is_int32() {
                    v.resize(new_len, filler.as_int32().unwrap());
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2.resize(new_len, filler);
                    }
                }
            }
            ElementsKind::Double(v) => {
                if filler.is_number() {
                    v.resize(new_len, filler.as_number().unwrap());
                } else if filler.is_int32() {
                    v.resize(new_len, filler.as_int32().unwrap() as f64);
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2.resize(new_len, filler);
                    }
                }
            }
            ElementsKind::Object(v) => {
                v.resize(new_len, filler);
            }
        }
    }

    pub fn truncate(&mut self, len: usize) {
        match self {
            ElementsKind::Smi(v) => v.truncate(len),
            ElementsKind::Double(v) => v.truncate(len),
            ElementsKind::Object(v) => v.truncate(len),
        }
    }

    pub fn shift(&mut self) -> Option<Value> {
        match self {
            ElementsKind::Smi(v) => {
                if v.is_empty() {
                    None
                } else {
                    Some(Value::int32(v.remove(0)))
                }
            }
            ElementsKind::Double(v) => {
                if v.is_empty() {
                    None
                } else {
                    Some(Value::number(v.remove(0)))
                }
            }
            ElementsKind::Object(v) => {
                if v.is_empty() {
                    None
                } else {
                    Some(v.remove(0))
                }
            }
        }
    }

    pub fn unshift(&mut self, value: Value) {
        match self {
            ElementsKind::Smi(v) => {
                if value.is_int32() {
                    v.insert(0, value.as_int32().unwrap());
                } else if value.is_number() {
                    let mut d: Vec<f64> = v.iter().map(|&x| x as f64).collect();
                    d.insert(0, value.as_number().unwrap());
                    *self = ElementsKind::Double(d);
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2.insert(0, value);
                    }
                }
            }
            ElementsKind::Double(v) => {
                if value.is_number() {
                    v.insert(0, value.as_number().unwrap());
                } else if value.is_int32() {
                    v.insert(0, value.as_int32().unwrap() as f64);
                } else {
                    self.transition_to_object();
                    if let ElementsKind::Object(v2) = self {
                        v2.insert(0, value);
                    }
                }
            }
            ElementsKind::Object(v) => {
                v.insert(0, value);
            }
        }
    }

    pub fn drain<R>(&mut self, range: R) -> Vec<Value>
    where
        R: std::ops::RangeBounds<usize> + Clone,
    {
        match self {
            ElementsKind::Smi(v) => v.drain(range).map(Value::int32).collect(),
            ElementsKind::Double(v) => v.drain(range).map(Value::number).collect(),
            ElementsKind::Object(v) => v.drain(range).collect(),
        }
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = Value> + '_> {
        match self {
            ElementsKind::Smi(v) => Box::new(v.iter().map(|&x| Value::int32(x))),
            ElementsKind::Double(v) => Box::new(v.iter().map(|&x| Value::number(x))),
            ElementsKind::Object(v) => {
                Box::new(v.iter().cloned()) as Box<dyn Iterator<Item = Value> + '_>
            }
        }
    }

    pub fn unwrap_object(&self) -> &Vec<Value> {
        match self {
            ElementsKind::Object(v) => v,
            _ => panic!("Expected Object elements kind"),
        }
    }

    fn transition_to_object(&mut self) {
        let new_vec = match self {
            ElementsKind::Smi(v) => v.iter().map(|&x| Value::int32(x)).collect(),
            ElementsKind::Double(v) => v.iter().map(|&x| Value::number(x)).collect(),
            ElementsKind::Object(_) => return,
        };
        *self = ElementsKind::Object(new_vec);
    }

    pub fn slice(&self, start: usize, end: usize) -> ElementsKind {
        match self {
            ElementsKind::Smi(v) => {
                let s = start.min(v.len());
                let e = end.min(v.len()).max(s);
                ElementsKind::Smi(v[s..e].to_vec())
            }
            ElementsKind::Double(v) => {
                let s = start.min(v.len());
                let e = end.min(v.len()).max(s);
                ElementsKind::Double(v[s..e].to_vec())
            }
            ElementsKind::Object(v) => {
                let s = start.min(v.len());
                let e = end.min(v.len()).max(s);
                ElementsKind::Object(v[s..e].to_vec())
            }
        }
    }

    pub fn reverse(&mut self) {
        match self {
            ElementsKind::Smi(v) => v.reverse(),
            ElementsKind::Double(v) => v.reverse(),
            ElementsKind::Object(v) => v.reverse(),
        }
    }

    pub fn append_all(&mut self, other: &ElementsKind) {
        match (self, other) {
            (ElementsKind::Smi(v1), ElementsKind::Smi(v2)) => v1.extend_from_slice(v2),
            (ElementsKind::Double(v1), ElementsKind::Double(v2)) => v1.extend_from_slice(v2),
            (ElementsKind::Object(v1), ElementsKind::Object(v2)) => {
                for val in v2 {
                    gc_write_barrier(val);
                }
                v1.extend_from_slice(v2);
            }
            (this, other) => {
                // Mixed types or transitioning
                for i in 0..other.len() {
                    this.push(other.get(i).unwrap_or(Value::hole()));
                }
            }
        }
    }

    pub fn splice(&mut self, start: usize, delete_count: usize, items: &[Value]) -> ElementsKind {
        let len = self.len();
        let actual_start = start.min(len);
        let actual_delete_count = delete_count.min(len - actual_start);
        let deleted = self.slice(actual_start, actual_start + actual_delete_count);

        if items.is_empty() {
            match self {
                ElementsKind::Smi(v) => {
                    v.drain(actual_start..actual_start + actual_delete_count);
                }
                ElementsKind::Double(v) => {
                    v.drain(actual_start..actual_start + actual_delete_count);
                }
                ElementsKind::Object(v) => {
                    v.drain(actual_start..actual_start + actual_delete_count);
                }
            }
            return deleted;
        }

        // Check compatibility
        let mut compatible = true;
        match self {
            ElementsKind::Smi(_) => {
                for item in items {
                    if !item.is_int32() {
                        compatible = false;
                        break;
                    }
                }
            }
            ElementsKind::Double(_) => {
                for item in items {
                    if !item.is_number() && !item.is_int32() {
                        compatible = false;
                        break;
                    }
                }
            }
            ElementsKind::Object(_) => {}
        }

        if !compatible {
            self.transition_to_object();
        }

        match self {
            ElementsKind::Smi(v) => {
                let smi_items: Vec<i32> = items.iter().map(|v| v.as_int32().unwrap()).collect();
                v.splice(actual_start..actual_start + actual_delete_count, smi_items);
            }
            ElementsKind::Double(v) => {
                let double_items: Vec<f64> = items
                    .iter()
                    .map(|v| {
                        if v.is_int32() {
                            v.as_int32().unwrap() as f64
                        } else {
                            v.as_number().unwrap()
                        }
                    })
                    .collect();
                v.splice(
                    actual_start..actual_start + actual_delete_count,
                    double_items,
                );
            }
            ElementsKind::Object(v) => {
                for item in items {
                    gc_write_barrier(item);
                }
                v.splice(
                    actual_start..actual_start + actual_delete_count,
                    items.iter().cloned(),
                );
            }
        }
        deleted
    }

    pub fn copy_within(&mut self, to: usize, from: usize, count: usize) {
        if count == 0 {
            return;
        }
        let len = self.len();
        if to >= len || from >= len {
            return;
        }
        let actual_count = count.min(len - to).min(len - from);

        match self {
            ElementsKind::Smi(v) => {
                v.copy_within(from..from + actual_count, to);
            }
            ElementsKind::Double(v) => {
                v.copy_within(from..from + actual_count, to);
            }
            ElementsKind::Object(v) => {
                v.copy_within(from..from + actual_count, to);
            }
        }
    }

    pub fn sort_with_comparator<F>(&mut self, mut compare: F)
    where
        F: FnMut(&Value, &Value) -> std::cmp::Ordering,
    {
        match self {
            ElementsKind::Smi(v) => {
                let mut values: Vec<Value> = v.iter().map(|&x| Value::int32(x)).collect();
                values.sort_by(|a, b| compare(a, b));
                *v = values.iter().map(|v| v.as_int32().unwrap_or(0)).collect();
            }
            ElementsKind::Double(v) => {
                let mut values: Vec<Value> = v.iter().map(|&x| Value::number(x)).collect();
                values.sort_by(|a, b| compare(a, b));
                *v = values
                    .iter()
                    .map(|v| v.as_number().unwrap_or(0.0))
                    .collect();
            }
            ElementsKind::Object(v) => {
                v.sort_by(|a, b| compare(a, b));
            }
        }
    }
}

/// The first `INLINE_PROPERTY_COUNT` properties are stored inline in the object
/// for faster access. Additional properties overflow to the `properties` Vec.
/// Both inline and overflow use flat Value slots + SlotMeta for storage.
pub struct JsObject {
    /// Current shape of the object
    shape: ObjectCell<Arc<Shape>>,
    /// Inline value slots for first N properties
    inline_slots: ObjectCell<[Value; INLINE_PROPERTY_COUNT]>,
    /// Inline metadata for first N properties
    inline_meta: ObjectCell<[SlotMeta; INLINE_PROPERTY_COUNT]>,
    /// Overflow value slots (for properties beyond INLINE_PROPERTY_COUNT)
    overflow_slots: ObjectCell<Vec<Value>>,
    /// Overflow metadata (parallel to overflow_slots)
    overflow_meta: ObjectCell<Vec<SlotMeta>>,
    /// Dictionary mode property storage (used when is_dictionary flag is set)
    /// When in dictionary mode, shape/inline/overflow are ignored for property access.
    dictionary_properties: ObjectCell<Option<IndexMap<PropertyKey, PropertyDescriptor>>>,
    /// Prototype (null for Object.prototype, mutable via Reflect.setPrototypeOf)
    /// Can be Value::object, Value::proxy, or Value::null
    prototype: ObjectCell<Value>,
    /// Array elements (for array-like objects)
    pub elements: ObjectCell<ElementsKind>,
    /// Object flags (mutable for freeze/seal/preventExtensions)
    pub flags: ObjectCell<ObjectFlags>,
    /// Parameter mapping for mapped arguments objects (sloppy mode).
    /// None for all non-arguments objects (8 bytes overhead).
    argument_mapping: ObjectCell<Option<Box<ArgumentMapping>>>,
}

/// Object flags
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjectFlags {
    /// Is this an array
    pub is_array: bool,
    /// Is extensible
    pub extensible: bool,
    /// Is sealed
    pub sealed: bool,
    /// Is frozen
    pub frozen: bool,
    /// Is in dictionary mode (HashMap storage, IC-uncacheable)
    pub is_dictionary: bool,
    /// Is an intrinsic/shared object (protected from teardown clearing)
    pub is_intrinsic: bool,
    /// Explicit array length, used when the array is sparse and elements.len()
    /// doesn't represent the true JS `.length`. `None` means use elements.len().
    pub sparse_array_length: Option<u32>,
    /// Cached dense elements length for fast `.length` reads when not sparse.
    pub dense_array_length_hint: u32,
    /// Whether array length is writable (None = true, default)
    pub array_length_writable: Option<bool>,
    /// String exotic object (new String("...")) — character indices are non-writable/non-configurable
    pub is_string_exotic: bool,
    /// [[IsHTMLDDA]] internal slot (Annex B). Objects with this flag:
    /// - typeof returns "undefined"
    /// - ToBoolean returns false
    /// - Abstract equality with null/undefined returns true
    pub is_htmldda: bool,
    /// Fast path: this is the Array.prototype.push native function
    pub is_array_push: bool,
    /// Fast path: this is the Array.prototype.pop native function
    pub is_array_pop: bool,
    /// Number of named property deletions performed on this object.
    /// Used to defer dictionary mode transition (slot-clearing is cheaper for 1-2 deletes).
    pub delete_count: u8,
    /// Value representing whether array is packed without holes
    pub is_packed: bool,
}

impl JsObject {
    /// Create a new empty object (prototype can be object, proxy, or null).
    ///
    /// Memory accounting is handled by `GcRef::new()` which calls
    /// `MemoryManager::current()` (thread-local) — no per-object Arc needed.
    pub fn new(prototype: Value) -> Self {
        Self {
            shape: ObjectCell::new(Shape::root()),
            inline_slots: ObjectCell::new([Value::undefined(); INLINE_PROPERTY_COUNT]),
            inline_meta: ObjectCell::new([SlotMeta::EMPTY; INLINE_PROPERTY_COUNT]),
            overflow_slots: ObjectCell::new(Vec::new()),
            overflow_meta: ObjectCell::new(Vec::new()),
            dictionary_properties: ObjectCell::new(None),
            prototype: ObjectCell::new(prototype),
            elements: ObjectCell::new(ElementsKind::new()),
            flags: ObjectCell::new(ObjectFlags {
                extensible: true,
                ..Default::default()
            }),
            argument_mapping: ObjectCell::new(None),
        }
    }

    /// Create an object with a pre-built shape and slot values.
    ///
    /// Used by JSON.parse fast path to avoid per-property shape transitions.
    /// `shape` must have exactly `values.len()` properties. Values are written
    /// directly into inline/overflow slots by offset. All slots are data+writable.
    pub(crate) fn with_shape_and_values(
        prototype: Value,
        shape: Arc<Shape>,
        values: &[Value],
    ) -> Self {
        let mut inline_slots = [Value::undefined(); INLINE_PROPERTY_COUNT];
        let mut inline_meta = [SlotMeta::EMPTY; INLINE_PROPERTY_COUNT];
        let overflow_count = values.len().saturating_sub(INLINE_PROPERTY_COUNT);
        let mut overflow_slots = Vec::with_capacity(overflow_count);
        let mut overflow_meta = Vec::with_capacity(overflow_count);

        for (i, val) in values.iter().enumerate() {
            gc_write_barrier(val);
            if i < INLINE_PROPERTY_COUNT {
                inline_slots[i] = *val;
                inline_meta[i] = SlotMeta::DEFAULT_DATA;
            } else {
                overflow_slots.push(*val);
                overflow_meta.push(SlotMeta::DEFAULT_DATA);
            }
        }

        Self {
            shape: ObjectCell::new(shape),
            inline_slots: ObjectCell::new(inline_slots),
            inline_meta: ObjectCell::new(inline_meta),
            overflow_slots: ObjectCell::new(overflow_slots),
            overflow_meta: ObjectCell::new(overflow_meta),
            dictionary_properties: ObjectCell::new(None),
            prototype: ObjectCell::new(prototype),
            elements: ObjectCell::new(ElementsKind::new()),
            flags: ObjectCell::new(ObjectFlags {
                extensible: true,
                ..Default::default()
            }),
            argument_mapping: ObjectCell::new(None),
        }
    }

    /// Fast variant for JSON.parse: skips GC write barriers because all values
    /// are freshly allocated in the current nursery generation (no old→young refs).
    ///
    /// SAFETY: Caller must guarantee all values were created in the current
    /// allocation cycle (no prior GC could have tenured any of them).
    pub(crate) fn with_shape_and_values_no_barrier(
        prototype: Value,
        shape: Arc<Shape>,
        values: &[Value],
    ) -> Self {
        let mut inline_slots = [Value::undefined(); INLINE_PROPERTY_COUNT];
        let mut inline_meta = [SlotMeta::EMPTY; INLINE_PROPERTY_COUNT];
        let overflow_count = values.len().saturating_sub(INLINE_PROPERTY_COUNT);
        let mut overflow_slots = Vec::with_capacity(overflow_count);
        let mut overflow_meta = Vec::with_capacity(overflow_count);

        for (i, val) in values.iter().enumerate() {
            if i < INLINE_PROPERTY_COUNT {
                inline_slots[i] = *val;
                inline_meta[i] = SlotMeta::DEFAULT_DATA;
            } else {
                overflow_slots.push(*val);
                overflow_meta.push(SlotMeta::DEFAULT_DATA);
            }
        }

        Self {
            shape: ObjectCell::new(shape),
            inline_slots: ObjectCell::new(inline_slots),
            inline_meta: ObjectCell::new(inline_meta),
            overflow_slots: ObjectCell::new(overflow_slots),
            overflow_meta: ObjectCell::new(overflow_meta),
            dictionary_properties: ObjectCell::new(None),
            prototype: ObjectCell::new(prototype),
            elements: ObjectCell::new(ElementsKind::new()),
            flags: ObjectCell::new(ObjectFlags {
                extensible: true,
                ..Default::default()
            }),
            argument_mapping: ObjectCell::new(None),
        }
    }

    /// Set up String exotic object: populate elements with characters and set flag.
    /// ES §10.4.3: String exotic objects expose character-index properties.
    pub fn setup_string_exotic(&self, s: &str) {
        let mut elements = self.elements.borrow_mut();
        elements.clear();
        for unit in s.encode_utf16() {
            let val = Value::string(JsString::intern_utf16(&[unit]));
            gc_write_barrier(&val);
            elements.push(val);
        }
        let len = elements.len() as u32;
        drop(elements);
        let mut flags = self.flags.borrow_mut();
        flags.is_string_exotic = true;
        flags.dense_array_length_hint = len;
    }

    /// Set the argument mapping for mapped arguments objects
    pub fn set_argument_mapping(&self, mapping: ArgumentMapping) {
        *self.argument_mapping.borrow_mut() = Some(Box::new(mapping));
    }

    /// Get the UpvalueCell for a mapped argument index, if any
    pub fn get_argument_cell(&self, index: usize) -> Option<UpvalueCell> {
        let mapping = self.argument_mapping.borrow();
        mapping
            .as_ref()
            .and_then(|m| m.cells.get(index))
            .and_then(|cell| cell.clone())
    }

    /// Unmap a specific argument index (e.g., after defineProperty with accessor)
    pub fn unmap_argument(&self, index: usize) {
        let mut mapping = self.argument_mapping.borrow_mut();
        if let Some(m) = mapping.as_mut() {
            if index < m.cells.len() {
                m.cells[index] = None;
            }
        }
    }

    /// Check if this object has any argument mapping
    pub fn has_argument_mapping(&self) -> bool {
        self.argument_mapping.borrow().is_some()
    }

    /// Get argument mapping for GC tracing (allocates)
    pub fn argument_mapping_cells(&self) -> Vec<UpvalueCell> {
        let mapping = self.argument_mapping.borrow();
        match mapping.as_ref() {
            Some(m) => m.cells.iter().filter_map(|c| c.clone()).collect(),
            None => Vec::new(),
        }
    }

    /// Trace argument mapping cells without allocating
    pub fn trace_argument_mapping(&self, tracer: &mut dyn crate::gc::Tracer) {
        use crate::gc::Trace;
        let mapping = self.argument_mapping.borrow();
        if let Some(m) = mapping.as_ref() {
            for cell in m.cells.iter().flatten() {
                cell.trace(tracer);
            }
        }
    }

    /// Create a new array
    pub fn array(length: usize) -> Self {
        let obj = Self::new(Value::null());
        // Cap dense element pre-allocation to avoid OOM on large sparse arrays.
        const MAX_DENSE_PREALLOC: usize = 1 << 24; // 16M elements
        let mut flags = obj.flags.borrow_mut();
        flags.is_array = true;
        flags.dense_array_length_hint = 0;
        flags.is_packed = length == 0;
        if length <= MAX_DENSE_PREALLOC {
            flags.dense_array_length_hint = length as u32;
            drop(flags);
            // Use holes, not undefined: `new Array(5)` creates 5 absent slots.
            // `0 in arr` → false, `arr[0]` → undefined (via get() hole handling).
            obj.elements.borrow_mut().resize(length, Value::hole());
        } else {
            flags.sparse_array_length = Some(length as u32);
            drop(flags);
        }
        obj
    }

    /// Create an array-like object (e.g., for `arguments`)
    ///
    /// This creates an object with indexed storage like an array,
    /// but is_array=false so Array.isArray() returns false.
    /// Per ES2026 §10.4.4, arguments objects are ordinary objects, not arrays.
    pub fn array_like(length: usize) -> Self {
        let obj = Self::new(Value::null());
        const MAX_DENSE_PREALLOC: usize = 1 << 24;
        if length <= MAX_DENSE_PREALLOC {
            obj.elements.borrow_mut().resize(length, Value::undefined());
            obj.flags.borrow_mut().dense_array_length_hint = length as u32;
        }
        // Note: is_array remains false (default)
        obj
    }

    /// Get property value by offset (for Inline Cache fast path)
    /// First INLINE_PROPERTY_COUNT properties are stored inline, rest in overflow.
    /// Returns None for accessor properties - caller should use get_property_entry_by_offset instead.
    #[inline]
    pub fn get_by_offset(&self, offset: usize) -> Option<Value> {
        if offset < INLINE_PROPERTY_COUNT {
            let meta = self.inline_meta.borrow();
            if meta[offset].is_data() {
                Some(self.inline_slots.borrow()[offset])
            } else {
                None
            }
        } else {
            let idx = offset - INLINE_PROPERTY_COUNT;
            let meta = self.overflow_meta.borrow();
            if idx < meta.len() && meta[idx].is_data() {
                self.overflow_slots.borrow().get(idx).copied()
            } else {
                None
            }
        }
    }

    /// Get property value by offset without borrow tracking overhead.
    ///
    /// # Safety
    ///
    /// Caller must guarantee there is no active mutable borrow of the
    /// corresponding property storage.
    #[inline]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn get_by_offset_unchecked(&self, offset: usize) -> Option<Value> {
        if offset < INLINE_PROPERTY_COUNT {
            let meta = unsafe { self.inline_meta.get_unchecked() };
            if meta[offset].is_data() {
                Some(unsafe { self.inline_slots.get_unchecked() }[offset])
            } else {
                None
            }
        } else {
            let idx = offset - INLINE_PROPERTY_COUNT;
            let meta = unsafe { self.overflow_meta.get_unchecked() };
            if idx < meta.len() && meta[idx].is_data() {
                unsafe { self.overflow_slots.get_unchecked() }
                    .get(idx)
                    .copied()
            } else {
                None
            }
        }
    }

    /// Get property entry by offset (includes accessor properties).
    /// Reconstructs a PropertyDescriptor from the slot value and metadata.
    #[inline]
    pub fn get_property_entry_by_offset(&self, offset: usize) -> Option<PropertyDescriptor> {
        if offset < INLINE_PROPERTY_COUNT {
            let meta = self.inline_meta.borrow();
            let m = meta[offset];
            if m.is_empty() {
                return None;
            }
            let slot = self.inline_slots.borrow()[offset];
            Self::descriptor_from_slot_meta(slot, m)
        } else {
            let idx = offset - INLINE_PROPERTY_COUNT;
            let meta = self.overflow_meta.borrow();
            if idx >= meta.len() {
                return None;
            }
            let m = meta[idx];
            if m.is_empty() {
                return None;
            }
            let slot = self
                .overflow_slots
                .borrow()
                .get(idx)
                .copied()
                .unwrap_or(Value::undefined());
            Self::descriptor_from_slot_meta(slot, m)
        }
    }

    /// Reconstruct a PropertyDescriptor from a slot value and its metadata.
    #[inline]
    fn descriptor_from_slot_meta(slot: Value, meta: SlotMeta) -> Option<PropertyDescriptor> {
        if meta.is_data() {
            Some(PropertyDescriptor::Data {
                value: slot,
                attributes: meta.to_attributes(),
            })
        } else if meta.is_accessor() {
            if let Some(pair) = slot.as_accessor_pair() {
                Some(PropertyDescriptor::Accessor {
                    get: if pair.getter.is_undefined() {
                        None
                    } else {
                        Some(pair.getter)
                    },
                    set: if pair.setter.is_undefined() {
                        None
                    } else {
                        Some(pair.setter)
                    },
                    attributes: meta.to_attributes(),
                })
            } else {
                // Malformed accessor slot — treat as absent
                None
            }
        } else {
            None
        }
    }

    /// Set property by offset (for Inline Cache fast path)
    /// First INLINE_PROPERTY_COUNT properties are stored inline, rest in overflow.
    #[inline]
    pub fn set_by_offset(&self, offset: usize, value: Value) -> Result<(), SetPropertyError> {
        let flags = self.flags.borrow();
        if flags.frozen {
            return Err(SetPropertyError::Frozen);
        }
        let is_sealed = flags.sealed;
        let is_extensible = flags.extensible;
        drop(flags);

        if offset < INLINE_PROPERTY_COUNT {
            let m = self.inline_meta.borrow()[offset];
            if m.is_data() {
                if m.is_writable() {
                    gc_write_barrier(&value);
                    self.inline_slots.borrow_mut()[offset] = value;
                    return Ok(());
                }
                return Err(SetPropertyError::NonWritable);
            } else if m.is_accessor() {
                return Err(SetPropertyError::AccessorWithoutSetter);
            } else {
                // Empty slot (deleted) — re-create as data property
                if !is_extensible {
                    return Err(SetPropertyError::NonExtensible);
                }
                if is_sealed {
                    return Err(SetPropertyError::Sealed);
                }
                gc_write_barrier(&value);
                self.inline_slots.borrow_mut()[offset] = value;
                self.inline_meta.borrow_mut()[offset] = SlotMeta::DEFAULT_DATA;
                return Ok(());
            }
        } else {
            let idx = offset - INLINE_PROPERTY_COUNT;
            let meta = self.overflow_meta.borrow();
            if idx < meta.len() {
                let m = meta[idx];
                drop(meta);
                if m.is_data() {
                    if m.is_writable() {
                        gc_write_barrier(&value);
                        self.overflow_slots.borrow_mut()[idx] = value;
                        return Ok(());
                    }
                    return Err(SetPropertyError::NonWritable);
                } else if m.is_accessor() {
                    return Err(SetPropertyError::AccessorWithoutSetter);
                } else {
                    // Empty slot (deleted) — re-create as data property
                    if !is_extensible {
                        return Err(SetPropertyError::NonExtensible);
                    }
                    if is_sealed {
                        return Err(SetPropertyError::Sealed);
                    }
                    gc_write_barrier(&value);
                    self.overflow_slots.borrow_mut()[idx] = value;
                    self.overflow_meta.borrow_mut()[idx] = SlotMeta::DEFAULT_DATA;
                    return Ok(());
                }
            }
            Err(SetPropertyError::NonExtensible)
        }
    }

    /// Get total property count (inline + overflow)
    #[allow(dead_code)]
    fn property_count(&self) -> usize {
        let inline_count = self
            .inline_meta
            .borrow()
            .iter()
            .filter(|m| !m.is_empty())
            .count();
        let overflow_count = self
            .overflow_meta
            .borrow()
            .iter()
            .filter(|m| !m.is_empty())
            .count();
        inline_count + overflow_count
    }

    /// Get current shape (clones Arc — atomic refcount bump).
    /// Prefer `with_shape()` or `shape_get_offset()` on hot paths.
    pub fn shape(&self) -> Arc<Shape> {
        self.shape.borrow().clone()
    }

    /// Access the shape by reference, without Arc clone.
    #[inline]
    pub fn with_shape<R>(&self, f: impl FnOnce(&Shape) -> R) -> R {
        let borrow = self.shape.borrow();
        f(&borrow)
    }

    /// Look up a property offset in the shape without Arc clone.
    #[inline]
    pub fn shape_get_offset(&self, key: &PropertyKey) -> Option<usize> {
        let borrow = self.shape.borrow();
        borrow.get_offset(key)
    }

    /// Get the raw pointer value of the current shape (for IC comparison).
    ///
    /// Returns `Arc::as_ptr()` without cloning the Arc, avoiding the atomic
    /// reference count increment/decrement overhead. The returned value is
    /// only valid for pointer comparison (not for dereferencing).
    /// Get unique shape ID.
    #[inline]
    pub(crate) fn shape_id(&self) -> u64 {
        self.shape.borrow().id
    }

    /// Get unique shape ID without borrow tracking overhead.
    ///
    /// # Safety
    ///
    /// Caller must guarantee no active mutable borrow of `shape`.
    #[inline]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn shape_id_unchecked(&self) -> u64 {
        unsafe { self.shape.get_unchecked() }.id
    }

    /// Get raw shape pointer... (for pointer comparison only)
    #[inline]
    pub(crate) fn shape_ptr_raw(&self) -> u64 {
        let borrow = self.shape.borrow();
        std::sync::Arc::as_ptr(&*borrow) as u64
    }

    /// Get raw shape pointer without borrow tracking overhead.
    ///
    /// # Safety
    ///
    /// Caller must guarantee no active mutable borrow of `shape`.
    #[inline]
    #[allow(unsafe_code)]
    pub(crate) unsafe fn shape_ptr_raw_unchecked(&self) -> u64 {
        std::sync::Arc::as_ptr(unsafe { self.shape.get_unchecked() }) as u64
    }

    /// Check if object is in dictionary mode (IC-uncacheable).
    /// Objects in dictionary mode use HashMap storage instead of shape-based indexed storage.
    #[inline]
    pub fn is_dictionary_mode(&self) -> bool {
        self.flags.borrow().is_dictionary
    }

    pub fn is_sparse(&self) -> bool {
        self.flags.borrow().sparse_array_length.is_some()
    }

    /// Debug: get the number of keys in the shape
    pub fn get_shape_key_count(&self) -> usize {
        self.shape.borrow().own_keys().len()
    }

    /// Debug: get number of non-empty inline property slots
    pub fn get_inline_occupied_count(&self) -> usize {
        self.inline_meta
            .borrow()
            .iter()
            .filter(|m| !m.is_empty())
            .count()
    }

    /// Transition object to dictionary mode.
    /// This converts shape-based indexed storage to HashMap storage.
    /// Called when property count exceeds DICTIONARY_THRESHOLD or on delete operations.
    fn transition_to_dictionary(&self) {
        let mut flags = self.flags.borrow_mut();
        if flags.is_dictionary {
            return; // Already in dictionary mode
        }

        // Build HashMap from existing properties (slots + meta → PropertyDescriptor)
        let mut dict = IndexMap::new();

        let shape = self.shape.borrow();
        let inline_slots = self.inline_slots.borrow();
        let inline_meta = self.inline_meta.borrow();
        let overflow_slots = self.overflow_slots.borrow();
        let overflow_meta = self.overflow_meta.borrow();

        // Iterate over all properties in the shape
        // IMPORTANT: Use the actual offset from shape, not a sequential counter
        for key in shape.own_keys() {
            if let Some(offset) = shape.get_offset(&key) {
                let (slot, meta) = if offset < INLINE_PROPERTY_COUNT {
                    (inline_slots[offset], inline_meta[offset])
                } else {
                    let idx = offset - INLINE_PROPERTY_COUNT;
                    if idx < overflow_slots.len() {
                        (overflow_slots[idx], overflow_meta[idx])
                    } else {
                        continue;
                    }
                };

                // Skip empty slots
                if meta.is_empty() {
                    continue;
                }

                if let Some(desc) = Self::descriptor_from_slot_meta(slot, meta) {
                    dict.insert(key, desc);
                }
            }
        }

        drop(shape);
        drop(inline_slots);
        drop(inline_meta);
        drop(overflow_slots);
        drop(overflow_meta);

        // Store the dictionary
        *self.dictionary_properties.borrow_mut() = Some(dict);
        // Replace shape with a fresh root — the unique Arc pointer invalidates
        // all IC entries that cached the old shape_id for this object.
        *self.shape.borrow_mut() = Shape::root();
        flags.is_dictionary = true;
    }

    /// Get property by key
    pub fn get(&self, key: &PropertyKey) -> Option<Value> {
        // Special handling for array "length" property.
        if self.is_array()
            && let PropertyKey::String(s) = key
            && s.as_str() == "length"
        {
            let len = self.array_length();
            if len <= i32::MAX as usize {
                return Some(Value::int32(len as i32));
            }
            return Some(Value::number(len as f64));
        }

        // String exotic objects: synthesize "length" and character indices
        if self.flags.borrow().is_string_exotic {
            if let PropertyKey::String(s) = key {
                if s.as_str() == "length" {
                    return Some(Value::int32(self.elements.borrow().len() as i32));
                }
            }
            if let PropertyKey::Index(i) = key {
                let elements = self.elements.borrow();
                let idx = *i as usize;
                if idx < elements.len() {
                    return Some(elements.get(idx).unwrap_or_else(Value::undefined));
                }
                return None;
            }
        }

        // Dictionary mode: use HashMap lookup
        if self.is_dictionary_mode() {
            if let Some(dict) = self.dictionary_properties.borrow().as_ref() {
                if let Some(desc) = dict.get(key) {
                    match desc {
                        PropertyDescriptor::Data { value, .. } => return Some(value.clone()),
                        PropertyDescriptor::Accessor { .. } => return None,
                        PropertyDescriptor::Deleted => {}
                    }
                }
            }
            // Fall through to indexed elements and prototype chain
        } else {
            // Check own properties first via shape lookup
            let shape = self.shape.borrow();
            if let Some(offset) = shape.get_offset(key) {
                if let Some(desc) = self.get_property_entry_by_offset(offset) {
                    match desc {
                        PropertyDescriptor::Data { value, .. } => return Some(value),
                        // Accessors are handled via `lookup_property_descriptor` in the interpreter.
                        // For this low-level helper, treat them as non-values.
                        PropertyDescriptor::Accessor { .. } => return None,
                        PropertyDescriptor::Deleted => {}
                    }
                }
            }
        }

        // Check indexed elements for arrays (holes resolve to None → undefined)
        if let PropertyKey::Index(i) = key {
            let idx = *i as usize;
            // For mapped arguments: read through UpvalueCell for aliased parameters
            if let Some(cell) = self.get_argument_cell(idx) {
                return Some(cell.get());
            }
            let elements = self.elements.borrow();
            if idx < elements.len() {
                let val = &elements.get(idx).unwrap_or(Value::undefined());
                if !val.is_hole() {
                    return Some(val.clone());
                }
                // Hole: fall through to prototype chain (returns None → undefined)
            }
            drop(elements);
            // For non-arrays, also try string property lookup
            let string_key = PropertyKey::String(crate::string::JsString::intern(&i.to_string()));
            return self.get(&string_key);
        }

        // Check prototype chain iteratively to avoid stack overflow
        let mut current_proto: Value = self.prototype.borrow().clone();
        let mut depth = 0;

        loop {
            // Handle different prototype types
            if let Some(proto_obj) = current_proto.as_object() {
                depth += 1;
                // Optimization/Safety: limit prototype chain depth
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    break;
                }

                // Check proto: dictionary mode first, then shape lookup
                if proto_obj.is_dictionary_mode() {
                    if let Some(dict) = proto_obj.dictionary_properties.borrow().as_ref() {
                        if let Some(desc) = dict.get(key) {
                            match desc {
                                PropertyDescriptor::Data { value, .. } => {
                                    return Some(value.clone());
                                }
                                PropertyDescriptor::Accessor { .. } => return None,
                                PropertyDescriptor::Deleted => {}
                            }
                        }
                    }
                } else {
                    let shape = proto_obj.shape.borrow();
                    if let Some(offset) = shape.get_offset(key) {
                        if let Some(desc) = proto_obj.get_property_entry_by_offset(offset) {
                            match desc {
                                PropertyDescriptor::Data { value, .. } => return Some(value),
                                PropertyDescriptor::Accessor { .. } => return None,
                                PropertyDescriptor::Deleted => {}
                            }
                        }
                    }
                }

                current_proto = proto_obj.prototype.borrow().clone();
            } else if let Some(proxy) = current_proto.as_proxy() {
                // Proxy in prototype chain - look at target transparently
                // Note: This bypasses proxy traps, which is incorrect per spec, but
                // JsObject::get() is a low-level helper without interpreter access.
                // Proper proxy handling should happen at higher levels (interpreter/intrinsics).
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    break;
                }
                if let Some(target) = proxy.target() {
                    current_proto = target;
                } else {
                    // Revoked proxy - end chain
                    break;
                }
            } else {
                // null, undefined, or other non-object - end of chain
                break;
            }
        }

        None
    }

    /// Extract all values held by this object and clear storage.
    /// Used for iterative destruction to prevent stack overflow.
    /// Intrinsic objects (shared across contexts) are protected and return empty.
    pub fn clear_and_extract_values(&self) -> Vec<Value> {
        // Intrinsic objects are shared across contexts and must not be cleared
        if self.is_intrinsic() {
            return Vec::new();
        }
        let mut values = Vec::new();

        // Clear inline slots
        {
            let mut slots = self.inline_slots.borrow_mut();
            let mut meta = self.inline_meta.borrow_mut();
            for i in 0..INLINE_PROPERTY_COUNT {
                if !meta[i].is_empty() {
                    let slot = slots[i];
                    if meta[i].is_accessor() {
                        if let Some(pair) = slot.as_accessor_pair() {
                            if !pair.getter.is_undefined() {
                                values.push(pair.getter);
                            }
                            if !pair.setter.is_undefined() {
                                values.push(pair.setter);
                            }
                        }
                    } else {
                        values.push(slot);
                    }
                    slots[i] = Value::undefined();
                    meta[i] = SlotMeta::EMPTY;
                }
            }
        }

        // Clear overflow slots
        {
            let mut slots = self.overflow_slots.borrow_mut();
            let mut meta = self.overflow_meta.borrow_mut();
            for i in 0..slots.len() {
                if !meta[i].is_empty() {
                    let slot = slots[i];
                    if meta[i].is_accessor() {
                        if let Some(pair) = slot.as_accessor_pair() {
                            if !pair.getter.is_undefined() {
                                values.push(pair.getter);
                            }
                            if !pair.setter.is_undefined() {
                                values.push(pair.setter);
                            }
                        }
                    } else {
                        values.push(slot);
                    }
                }
            }
            slots.clear();
            meta.clear();
        }

        // Clear elements
        {
            let mut elems = self.elements.borrow_mut();
            for val in elems.drain(..) {
                values.push(val);
            }
        }
        self.flags.borrow_mut().dense_array_length_hint = 0;

        // Clear prototype
        {
            let mut proto = self.prototype.borrow_mut();
            let proto_val = std::mem::replace(&mut *proto, Value::null());
            if !proto_val.is_null() && !proto_val.is_undefined() {
                values.push(proto_val);
            }
        }

        values
    }

    /// Get own property descriptor (does not walk prototype chain).
    pub fn get_own_property_descriptor(&self, key: &PropertyKey) -> Option<PropertyDescriptor> {
        // Array "length" property: synthesize as non-configurable, non-enumerable, writable
        if self.is_array() {
            if let PropertyKey::String(s) = key {
                if s.as_str() == "length" {
                    let flags = self.flags.borrow();
                    return Some(PropertyDescriptor::Data {
                        value: Value::number(self.array_length() as f64),
                        attributes: PropertyAttributes {
                            writable: !flags.frozen && flags.array_length_writable.unwrap_or(true),
                            enumerable: false,
                            configurable: false,
                        },
                    });
                }
            }
        }

        // ES §10.4.3.1: String exotic [[GetOwnProperty]] — synthesize
        // character-index descriptors and "length" for String wrapper objects.
        if self.flags.borrow().is_string_exotic {
            if let PropertyKey::String(s) = key {
                if s.as_str() == "length" {
                    let len = self.elements.borrow().len();
                    return Some(PropertyDescriptor::Data {
                        value: Value::number(len as f64),
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: false,
                            configurable: false,
                        },
                    });
                }
            }
            if let PropertyKey::Index(i) = key {
                let elements = self.elements.borrow();
                let idx = *i as usize;
                if idx < elements.len() {
                    return Some(PropertyDescriptor::Data {
                        value: elements.get(idx).unwrap_or_else(Value::undefined),
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: true,
                            configurable: false,
                        },
                    });
                }
                return None;
            }
        }

        // Dictionary mode: lookup in HashMap first (may contain accessor properties)
        if self.is_dictionary_mode() {
            if let Some(dict) = self.dictionary_properties.borrow().as_ref() {
                if let Some(desc) = dict.get(key) {
                    return Some(desc.clone());
                }
                // For Index keys, also try as String (e.g., Index(2) -> String("2"))
                if let PropertyKey::Index(i) = key {
                    let str_key = PropertyKey::string(&i.to_string());
                    if let Some(desc) = dict.get(&str_key) {
                        return Some(desc.clone());
                    }
                }
            }
            return None;
        }

        // Check shape first - it may contain accessor properties defined via Object.defineProperty
        let shape = self.shape.borrow();
        if let Some(offset) = shape.get_offset(key) {
            return self.get_property_entry_by_offset(offset);
        }
        // For Index keys, also try as String (e.g., Index(2) -> String("2"))
        // Note: Must use PropertyKey::String directly, not PropertyKey::string() which canonicalizes
        if let PropertyKey::Index(i) = key {
            let str_key = PropertyKey::String(JsString::intern(&i.to_string()));
            if let Some(offset) = shape.get_offset(&str_key) {
                return self.get_property_entry_by_offset(offset);
            }
        }
        drop(shape);

        // For mapped arguments: synthesize a data descriptor with the aliased value
        if let PropertyKey::Index(i) = key {
            let idx = *i as usize;
            if let Some(cell) = self.get_argument_cell(idx) {
                return Some(PropertyDescriptor::data(cell.get()));
            }
        }

        // Fall back to indexed elements as own data properties for ALL objects.
        // set() stores Index keys in elements for any object, so get_own_property_descriptor
        // must check elements for any object too (not just arrays).
        // Holes are treated as absent (return None).
        if let PropertyKey::Index(i) = key {
            let elements = self.elements.borrow();
            let idx = *i as usize;
            if idx < elements.len() && !elements.get(idx).unwrap_or(Value::undefined()).is_hole() {
                let flags = self.flags.borrow();
                return Some(PropertyDescriptor::Data {
                    value: elements.get(idx).unwrap_or_else(Value::undefined),
                    attributes: PropertyAttributes {
                        writable: !flags.frozen,
                        enumerable: true,
                        configurable: !(flags.sealed || flags.frozen),
                    },
                });
            }
        }

        None
    }

    /// Lookup property descriptor (walks prototype chain).
    pub fn lookup_property_descriptor(&self, key: &PropertyKey) -> Option<PropertyDescriptor> {
        if let Some(desc) = self.get_own_property_descriptor(key) {
            return Some(desc);
        }

        // Walk prototype chain iteratively to avoid stack overflow
        let mut current_proto: Value = self.prototype.borrow().clone();
        let mut depth = 0;

        loop {
            if let Some(proto_obj) = current_proto.as_object() {
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    return None; // Limit reached
                }

                if let Some(desc) = proto_obj.get_own_property_descriptor(key) {
                    return Some(desc);
                }
                // Also check elements for Index keys on any object
                // (set() stores Index values in elements for arrays, and has_own checks them,
                // but get_own_property_descriptor may miss them for non-array objects)
                if let PropertyKey::Index(i) = key {
                    let idx = *i as usize;
                    let elements = proto_obj.elements.borrow();
                    if idx < elements.len()
                        && !elements.get(idx).unwrap_or(Value::undefined()).is_hole()
                    {
                        let flags = proto_obj.flags.borrow();
                        return Some(PropertyDescriptor::Data {
                            value: elements.get(idx).unwrap_or_else(Value::undefined),
                            attributes: PropertyAttributes {
                                writable: !flags.frozen,
                                enumerable: true,
                                configurable: !(flags.sealed || flags.frozen),
                            },
                        });
                    }
                }

                current_proto = proto_obj.prototype.borrow().clone();
            } else if let Some(proxy) = current_proto.as_proxy() {
                // Proxy in prototype chain - look at target transparently
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    return None;
                }
                if let Some(target) = proxy.target() {
                    current_proto = target;
                } else {
                    // Revoked proxy - end chain
                    break;
                }
            } else {
                // null, undefined, or other - end of chain
                break;
            }
        }

        None
    }

    /// Set property by key
    pub fn set(&self, key: PropertyKey, value: Value) -> Result<(), SetPropertyError> {
        let flags = self.flags.borrow();
        let frozen = flags.frozen;
        let is_array = flags.is_array;
        let is_string_exotic = flags.is_string_exotic;
        let extensible = flags.extensible;
        let sealed = flags.sealed;
        let is_dictionary = flags.is_dictionary;
        drop(flags);

        // Frozen objects cannot have properties changed
        if frozen {
            return Err(SetPropertyError::Frozen);
        }

        // Array exotic: intercept `length` writes to truncate/extend
        if is_array {
            if let PropertyKey::String(s) = &key {
                if s.as_str() == "length" {
                    let new_len = crate::globals::to_number(&value);
                    if new_len < 0.0 || new_len != (new_len as u32 as f64) || new_len.is_nan() {
                        return Err(SetPropertyError::InvalidArrayLength);
                    }
                    if self.set_array_length(new_len as u32) {
                        return Ok(());
                    }
                    return Err(SetPropertyError::InvalidArrayLength);
                }
            }
        }

        // String exotic objects: character indices are non-writable
        if is_string_exotic {
            if let PropertyKey::Index(i) = &key {
                let idx = *i as usize;
                if idx < self.elements.borrow().len() {
                    return Err(SetPropertyError::NonWritable);
                }
            }
            if let PropertyKey::String(s) = &key {
                if s.as_str() == "length" {
                    return Err(SetPropertyError::NonWritable);
                }
            }
        }

        // Handle indexed elements for arrays
        if let PropertyKey::Index(i) = &key {
            let idx = *i as usize;
            // For mapped arguments: write through UpvalueCell for aliased parameters
            if let Some(cell) = self.get_argument_cell(idx) {
                gc_write_barrier(&value);
                cell.set(value.clone());
                // Also update elements for when mapping is later removed
                let mut elements = self.elements.borrow_mut();
                if idx < elements.len() {
                    elements.set(idx, value);
                }
                return Ok(());
            }
            let mut elements = self.elements.borrow_mut();
            if idx < elements.len() {
                gc_write_barrier(&value);
                elements.set(idx, value);
                return Ok(());
            } else if is_array && extensible && !sealed {
                // Cap dense element storage to avoid OOM on sparse arrays.
                // Indices beyond this limit are stored as dictionary properties.
                const MAX_DENSE_LENGTH: usize = 1 << 24; // 16M elements
                if idx < MAX_DENSE_LENGTH {
                    gc_write_barrier(&value);
                    elements.resize(idx + 1, Value::hole());
                    elements.set(idx, value);
                    self.flags.borrow_mut().dense_array_length_hint = elements.len() as u32;
                    return Ok(());
                }
                // Large sparse index — fall through to dictionary/string storage
            }
            drop(elements);
            // For non-arrays, fall through to store as string property
            let string_key = PropertyKey::String(crate::string::JsString::intern(&i.to_string()));
            return self.set(string_key, value);
        }

        // Dictionary mode: use HashMap storage
        if is_dictionary {
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                // If the property already exists, check writability and preserve attributes
                if let Some(existing) = map.get(&key) {
                    match existing {
                        PropertyDescriptor::Data { attributes, .. } => {
                            if !attributes.writable {
                                return Err(SetPropertyError::NonWritable);
                            }
                            // Preserve existing attributes, only update value
                            let attrs = *attributes;
                            gc_write_barrier(&value);
                            map.insert(
                                key,
                                PropertyDescriptor::Data {
                                    value,
                                    attributes: attrs,
                                },
                            );
                            return Ok(());
                        }
                        PropertyDescriptor::Accessor { .. } => {
                            return Err(SetPropertyError::AccessorWithoutSetter);
                        }
                        PropertyDescriptor::Deleted => {
                            // Deleted — treat as new property
                        }
                    }
                }
                // New property or deleted slot — use default attributes
                gc_write_barrier(&value);
                map.insert(key, PropertyDescriptor::data(value));
                return Ok(());
            }
            return Err(SetPropertyError::NonExtensible);
        }

        // Check if property exists
        {
            let shape = self.shape.borrow();
            if let Some(offset) = shape.get_offset(&key) {
                // Property exists, use set_by_offset
                drop(shape);
                return self.set_by_offset(offset, value);
            }
        }

        // New property addition
        if extensible && !sealed {
            let mut shape_write = self.shape.borrow_mut();
            // Transition to new shape
            let next_shape = shape_write.transition(key);
            let offset = next_shape
                .offset
                .expect("Shape transition should have an offset");

            // Check if we should transition to dictionary mode
            if offset >= DICTIONARY_THRESHOLD {
                drop(shape_write);
                self.transition_to_dictionary();
                // Now set in dictionary mode
                let mut dict = self.dictionary_properties.borrow_mut();
                if let Some(map) = dict.as_mut() {
                    gc_write_barrier(&value);
                    // Re-insert the key we were adding
                    map.insert(
                        next_shape.key.clone().unwrap(),
                        PropertyDescriptor::data(value),
                    );
                    return Ok(());
                }
                return Err(SetPropertyError::NonExtensible);
            }

            *shape_write = next_shape;

            gc_write_barrier(&value);

            if offset < INLINE_PROPERTY_COUNT {
                // Store in inline slot
                self.inline_slots.borrow_mut()[offset] = value;
                self.inline_meta.borrow_mut()[offset] = SlotMeta::DEFAULT_DATA;
            } else {
                // Store in overflow
                let overflow_idx = offset - INLINE_PROPERTY_COUNT;
                let mut slots = self.overflow_slots.borrow_mut();
                let mut meta = self.overflow_meta.borrow_mut();
                if overflow_idx >= slots.len() {
                    slots.resize(overflow_idx + 1, Value::undefined());
                    meta.resize(overflow_idx + 1, SlotMeta::EMPTY);
                }
                slots[overflow_idx] = value;
                meta[overflow_idx] = SlotMeta::DEFAULT_DATA;
            }
            Ok(())
        } else if !extensible {
            Err(SetPropertyError::NonExtensible)
        } else {
            Err(SetPropertyError::Sealed)
        }
    }

    /// Delete property
    pub fn delete(&self, key: &PropertyKey) -> bool {
        // String exotic objects: "length" property is non-configurable
        if self.flags.borrow().is_string_exotic {
            if let PropertyKey::String(s) = key {
                if s.as_str() == "length" {
                    return false;
                }
            }
        }
        // For mapped arguments: unmap the index on delete
        if let PropertyKey::Index(i) = key {
            self.unmap_argument(*i as usize);
        }
        // For index keys, first check if there's a non-configurable property descriptor.
        // This handles cases like Object.defineProperty(arr, '1', {configurable: false}).
        if let PropertyKey::Index(i) = key {
            // Check if there's a property descriptor in the shape (set via defineProperty)
            // Use string form of key since defineProperty stores with string key
            let str_key = PropertyKey::String(JsString::intern(&i.to_string()));
            if let Some(desc) = {
                let shape = self.shape.borrow();
                if let Some(offset) = shape.get_offset(&str_key) {
                    drop(shape);
                    self.get_property_entry_by_offset(offset)
                } else {
                    None
                }
            } {
                // Found a property descriptor - check if configurable
                if !desc.is_configurable() {
                    return false; // Cannot delete non-configurable property
                }
                // Configurable descriptor - proceed with deletion below
            }

            // String exotic objects: indexed properties within string length are non-configurable
            let idx = *i as usize;
            {
                let flags = self.flags.borrow();
                if flags.is_string_exotic {
                    let elements = self.elements.borrow();
                    if idx < elements.len() {
                        return false; // Cannot delete string character indices
                    }
                }
            }

            // Indexed element properties are non-configurable on sealed/frozen objects.
            // If the element exists as an own property, deletion must fail.
            let element_exists = {
                let elements = self.elements.borrow();
                idx < elements.len() && !elements.get(idx).unwrap_or(Value::undefined()).is_hole()
            };
            if element_exists {
                let flags = self.flags.borrow();
                if flags.sealed || flags.frozen {
                    return false;
                }
            }

            // ES §10.4.2.1 [[Delete]](P): Deleting an indexed property creates
            // a hole but NEVER changes the array's length. We set the element to
            // hole, then compact trailing holes to save memory, but preserve the
            // original length via sparse_array_length so array_length() stays
            // correct.
            let mut elements = self.elements.borrow_mut();
            if idx < elements.len() {
                self.flags.borrow_mut().is_packed = false;
                let original_len = elements.len();
                elements.set(idx, Value::hole());
                // Trim trailing holes to keep elements vec compact
                while elements
                    .get(elements.len().saturating_sub(1))
                    .map_or(false, |v| v.is_hole())
                {
                    elements.pop();
                }
                let new_len = elements.len();
                drop(elements); // Must drop before calling array_length()
                self.flags.borrow_mut().dense_array_length_hint = new_len as u32;
                // If trimming shortened the vec AND this is an array, preserve
                // the original length so array_length() returns the spec-correct value.
                // ES §10.4.2.1 [[Delete]](P): delete does NOT change length.
                if self.is_array() && new_len < original_len {
                    let len = self.array_length().max(original_len);
                    self.flags.borrow_mut().sparse_array_length = Some(len as u32);
                }
            }
            return true;
        }

        // Dictionary mode: remove from HashMap (but check configurable first)
        if self.is_dictionary_mode() {
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                if let Some(desc) = map.get(key) {
                    if !desc.is_configurable() {
                        return false;
                    }
                }
                map.shift_remove(key);
            }
            return true;
        }

        let Some(offset) = self.shape.borrow().get_offset(key) else {
            // Per JS semantics, deleting a non-existent property succeeds.
            return true;
        };

        // Already deleted => treat as absent.
        if self.get_property_entry_by_offset(offset).is_none() {
            return true;
        }

        // Sealed or frozen objects cannot have properties deleted.
        let flags = self.flags.borrow();
        if flags.sealed || flags.frozen {
            return false;
        }
        drop(flags);

        // Check if configurable
        if let Some(desc) = self.get_own_property_descriptor(key) {
            if !desc.is_configurable() {
                return false;
            }

            let mut flags = self.flags.borrow_mut();
            flags.delete_count = flags.delete_count.saturating_add(1);

            if flags.delete_count >= DELETE_DICTIONARY_THRESHOLD {
                drop(flags);
                // Too many deletes — transition to dictionary mode
                self.transition_to_dictionary();
                let mut dict = self.dictionary_properties.borrow_mut();
                if let Some(map) = dict.as_mut() {
                    map.shift_remove(key);
                }
            } else {
                drop(flags);
                // Clear the slot in-place (shape stays intact, IC remains valid)
                if offset < INLINE_PROPERTY_COUNT {
                    self.inline_slots.borrow_mut()[offset] = Value::undefined();
                    self.inline_meta.borrow_mut()[offset] = SlotMeta::EMPTY;
                } else {
                    let idx = offset - INLINE_PROPERTY_COUNT;
                    let mut slots = self.overflow_slots.borrow_mut();
                    let mut meta = self.overflow_meta.borrow_mut();
                    if idx < slots.len() {
                        slots[idx] = Value::undefined();
                    }
                    if idx < meta.len() {
                        meta[idx] = SlotMeta::EMPTY;
                    }
                }
            }
            return true;
        }

        // If we couldn't find storage for an offset that exists in the Shape,
        // treat it as a no-op deletion.
        true
    }

    /// Check if object has own property
    pub fn has_own(&self, key: &PropertyKey) -> bool {
        // Array "length" is a virtual property (synthesized, not stored in shape)
        if self.is_array() {
            if let PropertyKey::String(s) = key {
                if s.as_str() == "length" {
                    return true;
                }
            }
        }

        // String exotic objects: character indices and "length" are own properties
        if self.flags.borrow().is_string_exotic {
            if let PropertyKey::String(s) = key {
                if s.as_str() == "length" {
                    return true;
                }
            }
            if let PropertyKey::Index(i) = key {
                let idx = *i as usize;
                return idx < self.elements.borrow().len();
            }
        }

        // Dictionary mode: check HashMap
        if self.is_dictionary_mode() {
            if let Some(dict) = self.dictionary_properties.borrow().as_ref() {
                if dict.contains_key(key) {
                    return true;
                }
                // For Index keys, also try as String
                if let PropertyKey::Index(i) = key {
                    let str_key = PropertyKey::string(&i.to_string());
                    if dict.contains_key(&str_key) {
                        return true;
                    }
                }
            }
        } else {
            let shape = self.shape.borrow();
            if let Some(offset) = shape.get_offset(key) {
                if self.get_property_entry_by_offset(offset).is_some() {
                    return true;
                }
            }
            // For Index keys, also try as String (e.g., Index(1) -> String("1"))
            // because set() converts non-array Index keys to String for shape storage
            if let PropertyKey::Index(i) = key {
                let str_key = PropertyKey::String(JsString::intern(&i.to_string()));
                if let Some(offset) = shape.get_offset(&str_key) {
                    if self.get_property_entry_by_offset(offset).is_some() {
                        return true;
                    }
                }
            }
        }

        // Check indexed elements (holes are absent)
        if let PropertyKey::Index(i) = key {
            let elements = self.elements.borrow();
            let idx = *i as usize;
            return idx < elements.len()
                && !elements.get(idx).unwrap_or(Value::undefined()).is_hole();
        }

        false
    }

    /// Check if object has property (including prototype chain)
    pub fn has(&self, key: &PropertyKey) -> bool {
        if self.has_own(key) {
            return true;
        }

        // Walk prototype chain iteratively to avoid stack overflow
        let mut current_proto: Value = self.prototype.borrow().clone();
        let mut depth = 0;

        loop {
            if let Some(proto_obj) = current_proto.as_object() {
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    return false; // Limit reached
                }

                if proto_obj.has_own(key) {
                    return true;
                }

                current_proto = proto_obj.prototype.borrow().clone();
            } else if let Some(proxy) = current_proto.as_proxy() {
                // Proxy in prototype chain - look at target transparently
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    return false;
                }
                if let Some(target) = proxy.target() {
                    current_proto = target;
                } else {
                    // Revoked proxy - end chain
                    break;
                }
            } else {
                // null, undefined, or other - end of chain
                break;
            }
        }

        false
    }

    /// Get own property keys
    pub fn own_keys(&self) -> Vec<PropertyKey> {
        let mut integer_keys: Vec<u32> = Vec::new();
        let mut string_keys: Vec<PropertyKey> = Vec::new();

        // Dictionary mode: get keys from HashMap
        if self.is_dictionary_mode() {
            if let Some(dict) = self.dictionary_properties.borrow().as_ref() {
                for key in dict.keys() {
                    match key {
                        PropertyKey::Index(i) => integer_keys.push(*i),
                        PropertyKey::String(s) => {
                            // Check canonical array index form only.
                            if let Some(n) =
                                PropertyKey::parse_canonical_array_index_utf16(s.as_utf16())
                            {
                                integer_keys.push(n);
                            } else {
                                string_keys.push(key.clone());
                            }
                        }
                        _ => string_keys.push(key.clone()),
                    }
                }
            }
        } else {
            let shape_keys = self.shape.borrow().own_keys();
            for key in shape_keys {
                if self.get_own_property_descriptor(&key).is_some() {
                    match &key {
                        PropertyKey::Index(i) => integer_keys.push(*i),
                        PropertyKey::String(s) => {
                            // Check canonical array index form only.
                            if let Some(n) =
                                PropertyKey::parse_canonical_array_index_utf16(s.as_utf16())
                            {
                                integer_keys.push(n);
                            } else {
                                string_keys.push(key);
                            }
                        }
                        _ => string_keys.push(key),
                    }
                }
            }
        }

        // Add indexed elements (skip holes — they are absent per spec)
        let elements = self.elements.borrow();
        for i in 0..elements.len() {
            if !elements.get(i).unwrap_or(Value::undefined()).is_hole()
                && !integer_keys.contains(&(i as u32))
            {
                integer_keys.push(i as u32);
            }
        }

        // Sort integer keys numerically
        integer_keys.sort_unstable();

        // Build result: integer indices first, then string keys
        let mut keys = Vec::with_capacity(integer_keys.len() + string_keys.len());
        for i in integer_keys {
            keys.push(PropertyKey::Index(i));
        }
        keys.extend(string_keys);

        keys
    }

    /// ES2023 §9.1.6.3 ValidateAndApplyPropertyDescriptor
    ///
    /// Validates whether a property descriptor change is allowed.
    /// Returns Ok(true) if the change should proceed, Ok(false) if no change needed,
    /// or Err with a message if the change violates invariants.
    fn validate_property_descriptor_change(
        current: &PropertyDescriptor,
        desc: &PropertyDescriptor,
    ) -> Result<bool, &'static str> {
        // If current is Deleted, it's like a new property - always allowed
        if matches!(current, PropertyDescriptor::Deleted) {
            return Ok(true);
        }

        let current_configurable = current.is_configurable();

        // Get the new configurable value (default to current if not specified in desc)
        let new_configurable = desc.is_configurable();

        // If current is non-configurable, many changes are forbidden
        if !current_configurable {
            // Cannot change configurable from false to true
            if new_configurable {
                return Err("Cannot redefine non-configurable property as configurable");
            }

            // Cannot change enumerable on non-configurable property
            if current.enumerable() != desc.enumerable() {
                return Err("Cannot change enumerable on non-configurable property");
            }

            // Check data vs accessor conversion
            match (current, desc) {
                // Cannot convert data to accessor on non-configurable property
                (PropertyDescriptor::Data { .. }, PropertyDescriptor::Accessor { .. }) => {
                    return Err(
                        "Cannot convert data property to accessor on non-configurable property",
                    );
                }
                // Cannot convert accessor to data on non-configurable property
                (PropertyDescriptor::Accessor { .. }, PropertyDescriptor::Data { .. }) => {
                    return Err(
                        "Cannot convert accessor property to data on non-configurable property",
                    );
                }
                // Data to data: check writable constraints
                (
                    PropertyDescriptor::Data {
                        attributes: curr_attrs,
                        ..
                    },
                    PropertyDescriptor::Data {
                        attributes: new_attrs,
                        ..
                    },
                ) => {
                    // Cannot change writable from false to true on non-configurable property
                    if !curr_attrs.writable && new_attrs.writable {
                        return Err("Cannot make non-configurable non-writable property writable");
                    }
                    // Note: Changing value on non-configurable non-writable requires SameValue check
                    // which we skip for now as it requires value comparison
                }
                // Accessor to accessor: getter/setter changes require SameValue check
                // which we skip for now
                _ => {}
            }
        }

        Ok(true)
    }

    /// Define a property with descriptor
    pub fn define_property(&self, key: PropertyKey, desc: PropertyDescriptor) -> bool {
        // For mapped arguments: if defining an accessor descriptor on a mapped index, unmap it
        if let PropertyKey::Index(i) = &key {
            let idx = *i as usize;
            if self.get_argument_cell(idx).is_some() {
                if matches!(&desc, PropertyDescriptor::Accessor { .. }) {
                    self.unmap_argument(idx);
                } else if let PropertyDescriptor::Data { value, .. } = &desc {
                    // Data descriptor with explicit value: update through cell then unmap
                    if let Some(cell) = self.get_argument_cell(idx) {
                        cell.set(value.clone());
                    }
                }
            }
        }
        let flags = self.flags.borrow();

        // Frozen objects cannot have properties defined
        if flags.frozen {
            return false;
        }

        // Dictionary mode: store directly in HashMap
        if flags.is_dictionary {
            drop(flags);
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                // Validate change against existing property (if any)
                if let Some(existing) = map.get(&key) {
                    if Self::validate_property_descriptor_change(existing, &desc).is_err() {
                        return false;
                    }
                } else if !self.flags.borrow().extensible {
                    // Cannot add new property to non-extensible object
                    return false;
                }
                gc_write_barrier_desc(&desc);
                map.insert(key, desc);
                return true;
            }
            return false;
        }

        // Check if exists
        let offset = self.shape.borrow().get_offset(&key);

        if let Some(off) = offset {
            // Treat deleted slots as non-existent for extensibility checks.
            if self.get_property_entry_by_offset(off).is_none()
                && (!flags.extensible || flags.sealed)
            {
                return false;
            }

            // Update existing property - validate change
            if let Some(existing) = self.get_property_entry_by_offset(off) {
                if Self::validate_property_descriptor_change(&existing, &desc).is_err() {
                    return false;
                }
            }
            Self::write_desc_to_slot(self, off, &desc);
            return true;
        }

        // Can't add new properties if not extensible or sealed
        if !flags.extensible || flags.sealed {
            return false;
        }
        drop(flags);

        let mut shape_write = self.shape.borrow_mut();

        // Transition to new shape
        let next_shape = shape_write.transition(key.clone());
        let offset = next_shape
            .offset
            .expect("Shape transition should have an offset");

        // Check if we should transition to dictionary mode
        if offset >= DICTIONARY_THRESHOLD {
            drop(shape_write);
            self.transition_to_dictionary();
            // Store in dictionary
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                gc_write_barrier_desc(&desc);
                map.insert(key, desc);
                return true;
            }
            return false;
        }

        *shape_write = next_shape;

        Self::write_desc_to_slot(self, offset, &desc);
        true
    }

    /// Write a PropertyDescriptor into the flat slot + meta storage at `offset`.
    fn write_desc_to_slot(obj: &JsObject, offset: usize, desc: &PropertyDescriptor) {
        let (slot_val, meta) = Self::desc_to_slot_meta(desc);
        gc_write_barrier(&slot_val);

        if offset < INLINE_PROPERTY_COUNT {
            obj.inline_slots.borrow_mut()[offset] = slot_val;
            obj.inline_meta.borrow_mut()[offset] = meta;
        } else {
            let idx = offset - INLINE_PROPERTY_COUNT;
            let mut slots = obj.overflow_slots.borrow_mut();
            let mut metas = obj.overflow_meta.borrow_mut();
            if idx >= slots.len() {
                slots.resize(idx + 1, Value::undefined());
                metas.resize(idx + 1, SlotMeta::EMPTY);
            }
            slots[idx] = slot_val;
            metas[idx] = meta;
        }
    }

    /// Convert a PropertyDescriptor into a (Value, SlotMeta) pair for slot storage.
    fn desc_to_slot_meta(desc: &PropertyDescriptor) -> (Value, SlotMeta) {
        match desc {
            PropertyDescriptor::Data { value, attributes } => (*value, SlotMeta::data(*attributes)),
            PropertyDescriptor::Accessor {
                get,
                set,
                attributes,
            } => {
                let pair = GcRef::new(AccessorPair {
                    getter: get.unwrap_or(Value::undefined()),
                    setter: set.unwrap_or(Value::undefined()),
                });
                (Value::accessor_pair(pair), SlotMeta::accessor(*attributes))
            }
            PropertyDescriptor::Deleted => (Value::undefined(), SlotMeta::EMPTY),
        }
    }

    /// Store a property without validation checks.
    /// Used by `define_own_property` after it has already validated the operation.
    fn store_property(&self, key: PropertyKey, desc: PropertyDescriptor) {
        // Dictionary mode: store directly in HashMap
        if self.flags.borrow().is_dictionary {
            gc_write_barrier_desc(&desc);
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                map.insert(key, desc);
            }
            return;
        }

        let offset = self.shape.borrow().get_offset(&key);

        if let Some(off) = offset {
            // Update existing slot
            Self::write_desc_to_slot(self, off, &desc);
            return;
        }

        // New property: transition shape
        let mut shape_write = self.shape.borrow_mut();
        let next_shape = shape_write.transition(key.clone());
        let offset = next_shape
            .offset
            .expect("Shape transition should have an offset");

        if offset >= DICTIONARY_THRESHOLD {
            drop(shape_write);
            self.transition_to_dictionary();
            gc_write_barrier_desc(&desc);
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                map.insert(key, desc);
            }
            return;
        }

        *shape_write = next_shape;
        Self::write_desc_to_slot(self, offset, &desc);
    }

    /// [[DefineOwnProperty]] per ES2026 §10.1.6.1 / §10.1.6.3 (ValidateAndApplyPropertyDescriptor).
    ///
    /// Takes a `PartialDescriptor` where fields can be absent, and properly merges
    /// with the existing property descriptor. Returns false if the operation is rejected.
    pub fn define_own_property(&self, key: PropertyKey, desc: &PartialDescriptor) -> bool {
        use crate::intrinsics_impl::helpers::same_value;

        // Array exotic [[DefineOwnProperty]] (ES2026 §10.4.2.1)
        if self.is_array() {
            if let PropertyKey::String(ref s) = key {
                if s.as_str() == "length" {
                    return self.array_define_own_length(desc);
                }
            }
            // Check if key is an array index
            if let Some(index) = Self::to_array_index(&key) {
                return self.array_define_own_index(index, desc);
            }
        }

        // Handle mapped arguments: if defining an accessor on a mapped index, unmap it
        if let PropertyKey::Index(i) = &key {
            let idx = *i as usize;
            if self.get_argument_cell(idx).is_some() {
                if desc.is_accessor_descriptor() {
                    self.unmap_argument(idx);
                } else if let Some(ref val) = desc.value {
                    if let Some(cell) = self.get_argument_cell(idx) {
                        cell.set(val.clone());
                    }
                }
            }
        }

        let extensible = self.flags.borrow().extensible;
        let current = self.get_own_property_descriptor(&key);

        // Step 1-2: If property doesn't exist
        match current {
            None | Some(PropertyDescriptor::Deleted) => {
                // Step 2.a: If not extensible, return false
                if !extensible {
                    return false;
                }
                // Step 2.c-d: Create new property from partial with defaults
                let new_desc = if desc.is_accessor_descriptor() {
                    let get_val = desc.get.clone().unwrap_or(Value::undefined());
                    let set_val = desc.set.clone().unwrap_or(Value::undefined());
                    PropertyDescriptor::Accessor {
                        get: if get_val.is_undefined() {
                            None
                        } else {
                            Some(get_val)
                        },
                        set: if set_val.is_undefined() {
                            None
                        } else {
                            Some(set_val)
                        },
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: desc.enumerable.unwrap_or(false),
                            configurable: desc.configurable.unwrap_or(false),
                        },
                    }
                } else {
                    PropertyDescriptor::Data {
                        value: desc.value.clone().unwrap_or(Value::undefined()),
                        attributes: PropertyAttributes {
                            writable: desc.writable.unwrap_or(false),
                            enumerable: desc.enumerable.unwrap_or(false),
                            configurable: desc.configurable.unwrap_or(false),
                        },
                    }
                };
                self.store_property(key, new_desc);
                return true;
            }
            Some(ref existing) => {
                // Step 3: If every field in desc is absent, return true
                if desc.is_empty() {
                    return true;
                }

                let current_configurable = existing.is_configurable();
                let current_enumerable = existing.enumerable();

                // Step 4: If current is non-configurable...
                if !current_configurable {
                    // 4.a: Cannot make it configurable
                    if desc.configurable == Some(true) {
                        return false;
                    }
                    // 4.b: Cannot change enumerable
                    if let Some(new_enum) = desc.enumerable {
                        if new_enum != current_enumerable {
                            return false;
                        }
                    }
                }

                // Step 5: IsGenericDescriptor(desc) → just update attributes, valid for any type
                if desc.is_generic_descriptor() {
                    // Just merge enumerable/configurable, keep existing type
                    let merged = Self::merge_partial_with_current(existing, desc);
                    self.store_property(key, merged);
                    return true;
                }

                // Step 6: Type mismatch (data vs accessor)
                // By this point, generic descriptors have been handled (step 5).
                // Desc must be either data or accessor.
                let current_is_data = matches!(existing, PropertyDescriptor::Data { .. });
                let desc_is_data = desc.is_data_descriptor();

                if current_is_data != desc_is_data {
                    // Type conversion: data ↔ accessor
                    if !current_configurable {
                        return false;
                    }
                    // Convert, preserving configurable and enumerable from current
                    let merged = Self::merge_partial_with_current_converting(existing, desc);
                    self.store_property(key, merged);
                    return true;
                }

                // Step 7: Both data descriptors
                if current_is_data {
                    if let PropertyDescriptor::Data {
                        value: curr_val,
                        attributes: curr_attrs,
                    } = existing
                    {
                        if !current_configurable {
                            if !curr_attrs.writable {
                                // Step 7.a.i: Cannot make writable
                                if desc.writable == Some(true) {
                                    return false;
                                }
                                // Step 7.a.ii: Cannot change value (SameValue check)
                                if let Some(ref new_val) = desc.value {
                                    if !same_value(&curr_val, new_val) {
                                        return false;
                                    }
                                }
                            }
                        }
                    }
                    let merged = Self::merge_partial_with_current(existing, desc);
                    self.store_property(key, merged);
                    return true;
                }

                // Step 8: Both accessor descriptors (the only remaining case)
                if let PropertyDescriptor::Accessor {
                    get: curr_get,
                    set: curr_set,
                    ..
                } = &existing
                {
                    if !current_configurable {
                        // Step 8.a.i: Cannot change set
                        if let Some(ref new_set) = desc.set {
                            let curr_set_val = curr_set.clone().unwrap_or(Value::undefined());
                            if !same_value(&curr_set_val, new_set) {
                                return false;
                            }
                        }
                        // Step 8.a.ii: Cannot change get
                        if let Some(ref new_get) = desc.get {
                            let curr_get_val = curr_get.clone().unwrap_or(Value::undefined());
                            if !same_value(&curr_get_val, new_get) {
                                return false;
                            }
                        }
                    }
                }
                let merged = Self::merge_partial_with_current(existing, desc);
                self.store_property(key, merged);
                true
            }
        }
    }

    /// Merge a PartialDescriptor into an existing PropertyDescriptor, preserving
    /// current values for absent fields. Both are same type (data/data or accessor/accessor).
    fn merge_partial_with_current(
        current: &PropertyDescriptor,
        desc: &PartialDescriptor,
    ) -> PropertyDescriptor {
        match current {
            PropertyDescriptor::Data {
                value: curr_val,
                attributes: curr_attrs,
            } => {
                let new_value = desc.value.clone().unwrap_or_else(|| curr_val.clone());
                let new_writable = desc.writable.unwrap_or(curr_attrs.writable);
                let new_enumerable = desc.enumerable.unwrap_or(curr_attrs.enumerable);
                let new_configurable = desc.configurable.unwrap_or(curr_attrs.configurable);
                PropertyDescriptor::Data {
                    value: new_value,
                    attributes: PropertyAttributes {
                        writable: new_writable,
                        enumerable: new_enumerable,
                        configurable: new_configurable,
                    },
                }
            }
            PropertyDescriptor::Accessor {
                get: curr_get,
                set: curr_set,
                attributes: curr_attrs,
            } => {
                // For get/set: None in partial = absent (preserve current),
                // Some(undefined) = explicitly clear, Some(fn) = set to fn.
                let new_get = match &desc.get {
                    None => curr_get.clone(),
                    Some(v) if v.is_undefined() => None,
                    Some(v) => Some(v.clone()),
                };
                let new_set = match &desc.set {
                    None => curr_set.clone(),
                    Some(v) if v.is_undefined() => None,
                    Some(v) => Some(v.clone()),
                };
                let new_enumerable = desc.enumerable.unwrap_or(curr_attrs.enumerable);
                let new_configurable = desc.configurable.unwrap_or(curr_attrs.configurable);
                PropertyDescriptor::Accessor {
                    get: new_get,
                    set: new_set,
                    attributes: PropertyAttributes {
                        writable: false,
                        enumerable: new_enumerable,
                        configurable: new_configurable,
                    },
                }
            }
            PropertyDescriptor::Deleted => {
                // Shouldn't happen (caller checks), but handle gracefully
                if desc.is_accessor_descriptor() {
                    let get_val = desc.get.clone().unwrap_or(Value::undefined());
                    let set_val = desc.set.clone().unwrap_or(Value::undefined());
                    PropertyDescriptor::Accessor {
                        get: if get_val.is_undefined() {
                            None
                        } else {
                            Some(get_val)
                        },
                        set: if set_val.is_undefined() {
                            None
                        } else {
                            Some(set_val)
                        },
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: desc.enumerable.unwrap_or(false),
                            configurable: desc.configurable.unwrap_or(false),
                        },
                    }
                } else {
                    PropertyDescriptor::Data {
                        value: desc.value.clone().unwrap_or(Value::undefined()),
                        attributes: PropertyAttributes {
                            writable: desc.writable.unwrap_or(false),
                            enumerable: desc.enumerable.unwrap_or(false),
                            configurable: desc.configurable.unwrap_or(false),
                        },
                    }
                }
            }
        }
    }

    /// Merge with type conversion (data → accessor or accessor → data).
    /// Preserves enumerable/configurable from current, resets other fields to defaults.
    fn merge_partial_with_current_converting(
        current: &PropertyDescriptor,
        desc: &PartialDescriptor,
    ) -> PropertyDescriptor {
        let curr_enumerable = current.enumerable();
        let curr_configurable = current.is_configurable();

        if desc.is_accessor_descriptor() {
            // Converting to accessor
            let get_val = desc.get.clone().unwrap_or(Value::undefined());
            let set_val = desc.set.clone().unwrap_or(Value::undefined());
            PropertyDescriptor::Accessor {
                get: if get_val.is_undefined() {
                    None
                } else {
                    Some(get_val)
                },
                set: if set_val.is_undefined() {
                    None
                } else {
                    Some(set_val)
                },
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: desc.enumerable.unwrap_or(curr_enumerable),
                    configurable: desc.configurable.unwrap_or(curr_configurable),
                },
            }
        } else {
            // Converting to data
            PropertyDescriptor::Data {
                value: desc.value.clone().unwrap_or(Value::undefined()),
                attributes: PropertyAttributes {
                    writable: desc.writable.unwrap_or(false),
                    enumerable: desc.enumerable.unwrap_or(curr_enumerable),
                    configurable: desc.configurable.unwrap_or(curr_configurable),
                },
            }
        }
    }

    /// Convert a PropertyKey to an array index (u32), if it represents one.
    /// Per ES2026 §6.1.7: An array index is a String property key that is a
    /// canonical numeric string in the range 0..2^32-2.
    fn to_array_index(key: &PropertyKey) -> Option<u32> {
        match key {
            PropertyKey::Index(i) => {
                if *i <= 0xFFFF_FFFE {
                    Some(*i)
                } else {
                    None
                }
            }
            PropertyKey::String(s) => PropertyKey::parse_canonical_array_index_utf16(s.as_utf16()),
            PropertyKey::Symbol(_) => None,
        }
    }

    /// Array [[DefineOwnProperty]] for "length" (ES2026 §10.4.2.4)
    fn array_define_own_length(&self, desc: &PartialDescriptor) -> bool {
        use crate::intrinsics_impl::helpers::same_value;

        let old_len = self.array_length() as u32;
        let old_len_writable = self.flags.borrow().array_length_writable.unwrap_or(true);

        // If desc has no value, treat as ordinary defineOwnProperty on length
        let new_len_val = match &desc.value {
            None => {
                // Just updating attributes on length
                if let Some(false) = desc.writable {
                    self.flags.borrow_mut().array_length_writable = Some(false);
                }
                return true;
            }
            Some(v) => v.clone(),
        };

        // ToUint32
        let new_len = if let Some(n) = new_len_val.as_number() {
            let uint32 = n as u32;
            if (uint32 as f64) != n {
                return false; // Would be RangeError in calling code
            }
            uint32
        } else if new_len_val.is_undefined() {
            0
        } else {
            return false;
        };

        // Build a new PartialDescriptor with the uint32 value
        let new_desc = PartialDescriptor {
            value: Some(Value::number(new_len as f64)),
            writable: desc.writable,
            enumerable: desc.enumerable,
            configurable: desc.configurable,
            get: None,
            set: None,
        };

        if new_len >= old_len {
            // Growing or same: just validate and set
            if !old_len_writable {
                // Non-writable length: can only succeed if value is same
                if let Some(ref v) = new_desc.value {
                    if !same_value(&Value::number(old_len as f64), v) {
                        return false;
                    }
                }
            }
            self.set_array_length(new_len);
            if let Some(false) = new_desc.writable {
                self.flags.borrow_mut().array_length_writable = Some(false);
            }
            return true;
        }

        // Shrinking
        if !old_len_writable {
            return false;
        }
        let new_writable = new_desc.writable.unwrap_or(true);
        self.set_array_length(new_len);
        if !new_writable {
            self.flags.borrow_mut().array_length_writable = Some(false);
        }
        true
    }

    /// Array [[DefineOwnProperty]] for an array index (ES2026 §10.4.2.1 step 3)
    fn array_define_own_index(&self, index: u32, desc: &PartialDescriptor) -> bool {
        let old_len = self.array_length() as u32;
        let length_writable = self.flags.borrow().array_length_writable.unwrap_or(true);

        // Step 3.b: If index >= oldLen and length is not writable, return false
        if index >= old_len && !length_writable {
            return false;
        }

        // For accessor descriptors on indexed properties, store via shape (not elements)
        if desc.is_accessor_descriptor() {
            let key = PropertyKey::Index(index);
            // Delegate to ordinary [[DefineOwnProperty]] on this key
            // but bypass our array check (already handled)
            let result = self.ordinary_define_own_property(key, desc);
            // Step 3.f: If index >= oldLen, update length
            if result && index >= old_len {
                self.set_array_length(index + 1);
            }
            return result;
        }

        // For simple data properties on arrays, store in elements array
        let idx = index as usize;
        let value = desc.value.clone().unwrap_or(Value::undefined());

        // Check if element already exists in elements
        let elements_len = self.elements.borrow().len();
        if idx < elements_len {
            // Element exists - check current descriptor from shape first
            let key = PropertyKey::Index(index);
            if let Some(_existing) = {
                // Check shape for non-default attributes
                let shape = self.shape.borrow();
                shape
                    .get_offset(&key)
                    .and_then(|off| self.get_property_entry_by_offset(off))
            } {
                // Has explicit descriptor in shape - use ordinary path
                return self.ordinary_define_own_property(key, desc);
            }
            // Default data property in elements - just update value
            if self
                .elements
                .borrow()
                .get(idx)
                .map(|v| !v.is_hole())
                .unwrap_or(false)
            {
                // Existing element with default attributes
                // Check if desc specifies non-default attributes
                if desc.has_non_default_data_attributes() {
                    // Need to store in shape for custom attributes
                    return self.ordinary_define_own_property(key, desc);
                }
                gc_write_barrier(&value);
                self.elements.borrow_mut().set(idx, value);
            } else {
                // Hole - create new element
                if !self.flags.borrow().extensible {
                    return false;
                }
                if desc.has_non_default_data_attributes() {
                    return self.ordinary_define_own_property(key, desc);
                }
                gc_write_barrier(&value);
                self.elements.borrow_mut().set(idx, value);
            }
        } else {
            // Beyond current elements
            if !self.flags.borrow().extensible {
                return false;
            }
            // Sparse threshold: avoid allocating billions of holes for large indices
            const MAX_DENSE_PREALLOC: usize = 1 << 24; // 16M elements
            if desc.has_non_default_data_attributes() || idx >= MAX_DENSE_PREALLOC {
                let key = PropertyKey::Index(index);
                let result = self.ordinary_define_own_property(key, desc);
                if result && index >= old_len {
                    self.set_array_length(index + 1);
                }
                return result;
            }
            // Extend elements array (dense path)
            let mut elements = self.elements.borrow_mut();
            if idx > elements.len() {
                elements.resize(idx, Value::hole());
            }
            gc_write_barrier(&value);
            elements.push(value);
            self.flags.borrow_mut().dense_array_length_hint = elements.len() as u32;
        }

        // Step 3.f: If index >= oldLen, update length
        if index >= old_len {
            self.set_array_length(index + 1);
        }
        true
    }

    /// Ordinary [[DefineOwnProperty]] without array special handling.
    /// Used by array_define_own_index when it needs to fall through to the base algorithm.
    fn ordinary_define_own_property(&self, key: PropertyKey, desc: &PartialDescriptor) -> bool {
        use crate::intrinsics_impl::helpers::same_value;

        let extensible = self.flags.borrow().extensible;
        let current = self.get_own_property_descriptor(&key);

        match current {
            None | Some(PropertyDescriptor::Deleted) => {
                if !extensible {
                    return false;
                }
                let new_desc = if desc.is_accessor_descriptor() {
                    let get_val = desc.get.clone().unwrap_or(Value::undefined());
                    let set_val = desc.set.clone().unwrap_or(Value::undefined());
                    PropertyDescriptor::Accessor {
                        get: if get_val.is_undefined() {
                            None
                        } else {
                            Some(get_val)
                        },
                        set: if set_val.is_undefined() {
                            None
                        } else {
                            Some(set_val)
                        },
                        attributes: PropertyAttributes {
                            writable: false,
                            enumerable: desc.enumerable.unwrap_or(false),
                            configurable: desc.configurable.unwrap_or(false),
                        },
                    }
                } else {
                    PropertyDescriptor::Data {
                        value: desc.value.clone().unwrap_or(Value::undefined()),
                        attributes: PropertyAttributes {
                            writable: desc.writable.unwrap_or(false),
                            enumerable: desc.enumerable.unwrap_or(false),
                            configurable: desc.configurable.unwrap_or(false),
                        },
                    }
                };
                self.store_property(key, new_desc);
                true
            }
            Some(ref existing) => {
                if desc.is_empty() {
                    return true;
                }
                let current_configurable = existing.is_configurable();
                if !current_configurable {
                    if desc.configurable == Some(true) {
                        return false;
                    }
                    if let Some(new_enum) = desc.enumerable {
                        if new_enum != existing.enumerable() {
                            return false;
                        }
                    }
                }
                if desc.is_generic_descriptor() {
                    let merged = Self::merge_partial_with_current(existing, desc);
                    self.store_property(key, merged);
                    return true;
                }
                let current_is_data = matches!(existing, PropertyDescriptor::Data { .. });
                let desc_is_data = desc.is_data_descriptor();
                if current_is_data != desc_is_data {
                    if !current_configurable {
                        return false;
                    }
                    let merged = Self::merge_partial_with_current_converting(existing, desc);
                    self.store_property(key, merged);
                    return true;
                }
                if current_is_data {
                    if let PropertyDescriptor::Data {
                        value: curr_val,
                        attributes: curr_attrs,
                    } = existing
                    {
                        if !current_configurable && !curr_attrs.writable {
                            if desc.writable == Some(true) {
                                return false;
                            }
                            if let Some(ref new_val) = desc.value {
                                if !same_value(&curr_val, new_val) {
                                    return false;
                                }
                            }
                        }
                    }
                    let merged = Self::merge_partial_with_current(existing, desc);
                    self.store_property(key, merged);
                    return true;
                }
                // Both accessor descriptors
                if let PropertyDescriptor::Accessor {
                    get: curr_get,
                    set: curr_set,
                    ..
                } = &existing
                {
                    if !current_configurable {
                        if let Some(ref new_set) = desc.set {
                            let curr_set_val = curr_set.clone().unwrap_or(Value::undefined());
                            if !same_value(&curr_set_val, new_set) {
                                return false;
                            }
                        }
                        if let Some(ref new_get) = desc.get {
                            let curr_get_val = curr_get.clone().unwrap_or(Value::undefined());
                            if !same_value(&curr_get_val, new_get) {
                                return false;
                            }
                        }
                    }
                }
                let merged = Self::merge_partial_with_current(existing, desc);
                self.store_property(key, merged);
                true
            }
        }
    }

    /// Get prototype
    pub fn prototype(&self) -> Value {
        self.prototype.borrow().clone()
    }

    /// Set prototype
    /// Returns false if object is not extensible, if it would create a cycle,
    /// or if the chain would be too deep
    pub fn set_prototype(&self, prototype: Value) -> bool {
        if !self.flags.borrow().extensible {
            return false;
        }

        // Check for cycles and excessive depth
        let self_ptr = self as *const JsObject;
        let mut current_proto = prototype.clone();
        let mut depth = 0;

        loop {
            if let Some(proto_obj) = current_proto.as_object() {
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    return false; // Chain would be too deep
                }
                if proto_obj.as_ptr() == self_ptr {
                    return false; // Would create cycle
                }
                current_proto = proto_obj.prototype.borrow().clone();
            } else if let Some(proxy) = current_proto.as_proxy() {
                // Proxy in prototype chain - check its target for cycles
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    return false;
                }
                // Get proxy target and continue checking
                if let Some(target) = proxy.target() {
                    current_proto = target;
                } else {
                    // Revoked proxy - end chain
                    break;
                }
            } else {
                // null, undefined, or other - end of chain
                break;
            }
        }

        gc_write_barrier(&prototype);
        *self.prototype.borrow_mut() = prototype;
        // Bump global proto epoch to invalidate any cached prototype chain lookups
        bump_proto_epoch();
        true
    }

    /// Check if object is an array
    pub fn is_array(&self) -> bool {
        self.flags.borrow().is_array
    }

    /// Check if object has [[IsHTMLDDA]] internal slot (Annex B)
    pub fn is_htmldda(&self) -> bool {
        self.flags.borrow().is_htmldda
    }

    /// Check if array is packed (no holes)
    pub fn is_packed(&self) -> bool {
        self.flags.borrow().is_packed
    }

    /// Mark this object as an array exotic object
    /// Used for Array.prototype per ES2026 §23.1.3
    pub fn mark_as_array(&self) {
        let mut flags = self.flags.borrow_mut();
        flags.is_array = true;
        flags.dense_array_length_hint = self.elements.borrow().len() as u32;
    }

    // ========================================================================
    // Object.freeze / Object.seal / Object.preventExtensions
    // ========================================================================

    /// Freeze the object - makes all properties non-writable and non-configurable,
    /// and prevents adding new properties
    pub fn freeze(&self) {
        let mut flags = self.flags.borrow_mut();
        flags.frozen = true;
        flags.sealed = true;
        flags.extensible = false;
        if flags.is_array {
            flags.array_length_writable = Some(false);
        }
        drop(flags);

        // Make all inline properties non-writable and non-configurable
        {
            let mut meta = self.inline_meta.borrow_mut();
            for m in meta.iter_mut() {
                if m.is_data() {
                    *m = m.with_writable(false).with_configurable(false);
                } else if m.is_accessor() {
                    *m = m.with_configurable(false);
                }
            }
        }

        // Make all overflow properties non-writable and non-configurable
        {
            let mut meta = self.overflow_meta.borrow_mut();
            for m in meta.iter_mut() {
                if m.is_data() {
                    *m = m.with_writable(false).with_configurable(false);
                } else if m.is_accessor() {
                    *m = m.with_configurable(false);
                }
            }
        }

        // Make all dictionary-mode properties non-writable and non-configurable
        if let Some(dict) = self.dictionary_properties.borrow_mut().as_mut() {
            for desc in dict.values_mut() {
                match desc {
                    PropertyDescriptor::Data { attributes, .. } => {
                        attributes.writable = false;
                        attributes.configurable = false;
                    }
                    PropertyDescriptor::Accessor { attributes, .. } => {
                        attributes.configurable = false;
                    }
                    PropertyDescriptor::Deleted => {}
                }
            }
        }
    }

    /// Check if object is frozen per ES2024 §20.1.2.13:
    /// Not extensible AND all own properties non-configurable AND all data properties non-writable.
    pub fn is_frozen(&self) -> bool {
        let flags = self.flags.borrow();
        // Fast path: if freeze() was called, all properties were already modified
        if flags.frozen {
            return true;
        }
        // If extensible, cannot be frozen
        if flags.extensible {
            return false;
        }
        drop(flags);
        // Slow path: check every own property
        for key in self.own_keys() {
            if let Some(desc) = self.get_own_property_descriptor(&key) {
                if desc.is_configurable() {
                    return false;
                }
                if let PropertyDescriptor::Data { attributes, .. } = &desc {
                    if attributes.writable {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Seal the object - prevents adding new properties and makes all existing
    /// properties non-configurable
    pub fn seal(&self) {
        let mut flags = self.flags.borrow_mut();
        flags.sealed = true;
        flags.extensible = false;
        drop(flags);

        // Make all inline properties non-configurable
        {
            let mut meta = self.inline_meta.borrow_mut();
            for m in meta.iter_mut() {
                if !m.is_empty() {
                    *m = m.with_configurable(false);
                }
            }
        }

        // Make all overflow properties non-configurable
        {
            let mut meta = self.overflow_meta.borrow_mut();
            for m in meta.iter_mut() {
                if !m.is_empty() {
                    *m = m.with_configurable(false);
                }
            }
        }

        // Make all dictionary-mode properties non-configurable
        if let Some(dict) = self.dictionary_properties.borrow_mut().as_mut() {
            for desc in dict.values_mut() {
                match desc {
                    PropertyDescriptor::Data { attributes, .. } => {
                        attributes.configurable = false;
                    }
                    PropertyDescriptor::Accessor { attributes, .. } => {
                        attributes.configurable = false;
                    }
                    PropertyDescriptor::Deleted => {}
                }
            }
        }
    }

    /// Check if object is sealed per ES2024 §20.1.2.15:
    /// Not extensible AND all own properties non-configurable.
    pub fn is_sealed(&self) -> bool {
        let flags = self.flags.borrow();
        // Fast path: if seal() was called, all properties were already modified
        if flags.sealed {
            return true;
        }
        // If extensible, cannot be sealed
        if flags.extensible {
            return false;
        }
        drop(flags);
        // Slow path: check every own property
        for key in self.own_keys() {
            if let Some(desc) = self.get_own_property_descriptor(&key) {
                if desc.is_configurable() {
                    return false;
                }
            }
        }
        true
    }

    /// Prevent extensions - prevents adding new properties
    pub fn prevent_extensions(&self) {
        self.flags.borrow_mut().extensible = false;
    }

    /// Check if object is extensible
    pub fn is_extensible(&self) -> bool {
        self.flags.borrow().extensible
    }

    pub fn array_length_writable(&self) -> bool {
        self.flags.borrow().array_length_writable.unwrap_or(true)
    }

    pub fn is_proxy_pure_object(&self) -> bool {
        // This is a placeholder since JsObject doesn't store is_proxy currently.
        // In Otter, Proxies are Value::Proxy(GcRef<Proxy>).
        false
    }

    /// Mark this object as intrinsic (shared across contexts, protected from teardown clearing)
    pub fn mark_as_intrinsic(&self) {
        self.flags.borrow_mut().is_intrinsic = true;
    }

    /// Check if this object is an intrinsic
    pub fn is_intrinsic(&self) -> bool {
        self.flags.borrow().is_intrinsic
    }

    /// Set array length (exotic [[DefineOwnProperty]] behavior per ES2023 §10.4.2.4).
    ///
    /// - If `new_len < current_len`: truncate elements to `new_len`
    /// - If `new_len > current_len`: extend with holes (absent elements)
    pub fn set_array_length(&self, new_len: u32) -> bool {
        let current_len = self.array_length() as u32;
        if new_len == current_len {
            return true;
        }

        const MAX_DENSE_PREALLOC: usize = 1 << 24; // 16M elements

        if new_len < current_len {
            // Truncate
            let mut elements = self.elements.borrow_mut();
            let dense_len = elements.len();
            let target = (new_len as usize).min(dense_len);
            elements.truncate(target);
            // Update sparse_array_length if it was set
            let mut flags = self.flags.borrow_mut();
            flags.dense_array_length_hint = elements.len() as u32;
            if flags.sparse_array_length.is_some() {
                if (new_len as usize) <= elements.len() {
                    flags.sparse_array_length = None;
                } else {
                    flags.sparse_array_length = Some(new_len);
                }
            }
        } else {
            // Extend with holes
            self.flags.borrow_mut().is_packed = false;
            let new = new_len as usize;
            if new <= MAX_DENSE_PREALLOC {
                self.elements.borrow_mut().resize(new, Value::hole());
                let mut flags = self.flags.borrow_mut();
                flags.sparse_array_length = None;
                flags.dense_array_length_hint = new_len;
            } else {
                self.flags.borrow_mut().sparse_array_length = Some(new_len);
            }
        }
        true
    }

    /// Get array length (for arrays)
    pub fn array_length(&self) -> usize {
        let flags = self.flags.borrow();
        if let Some(sparse_len) = flags.sparse_array_length {
            return sparse_len as usize;
        }
        if flags.is_array {
            return flags.dense_array_length_hint as usize;
        }
        drop(flags);
        self.elements.borrow().len()
    }

    /// Fast path for getting an element by index.
    /// Returns None if the index is out of bounds or it's a hole.
    pub fn get_index(&self, index: usize) -> Option<Value> {
        // Fast path for non-mapped arguments: avoid borrow_mut and logic
        // This covers 99.9% of arrays.
        let elements = self.elements.borrow();
        if index < elements.len() {
            let val = &elements.get(index).unwrap_or(Value::undefined());
            if !val.is_hole() {
                return Some(val.clone());
            }
        }
        drop(elements);

        // Slow path: check for mapped arguments (aliased parameters)
        if let Some(cell) = self.get_argument_cell(index) {
            return Some(cell.get());
        }

        None
    }

    /// Fast path for setting an element by index.
    /// Handles length updates and sparse-to-dense transitions.
    pub fn set_index(&self, index: usize, value: Value) -> Result<(), SetPropertyError> {
        let (is_frozen, is_extensible, is_sealed) = {
            let flags = self.flags.borrow();
            (flags.frozen, flags.extensible, flags.sealed)
        };
        if is_frozen {
            return Err(SetPropertyError::Frozen);
        }

        let mut elements = self.elements.borrow_mut();
        if index < elements.len() {
            gc_write_barrier(&value);
            elements.set(index, value);
            return Ok(());
        }
        drop(elements);

        // For mapped arguments: write through UpvalueCell for aliased parameters
        if let Some(cell) = self.get_argument_cell(index) {
            gc_write_barrier(&value);
            cell.set(value.clone());
            // Also update elements for when mapping is later removed
            let mut elements = self.elements.borrow_mut();
            if index < elements.len() {
                elements.set(index, value);
            }
            return Ok(());
        }

        let mut elements = self.elements.borrow_mut();
        if index < elements.len() {
            gc_write_barrier(&value);
            elements.set(index, value);
            return Ok(());
        }

        if is_extensible && !is_sealed {
            // Cap dense element storage to avoid OOM on sparse arrays.
            const MAX_DENSE_LENGTH: usize = 1 << 24; // 16M elements
            if index < MAX_DENSE_LENGTH {
                gc_write_barrier(&value);
                if index > elements.len() {
                    self.flags.borrow_mut().is_packed = false;
                }
                elements.resize(index + 1, Value::hole());
                elements.set(index, value);
                self.flags.borrow_mut().dense_array_length_hint = elements.len() as u32;
                return Ok(());
            }
        }

        drop(elements);

        // Fallback to generic set for large sparse indices
        self.set(PropertyKey::Index(index as u32), value)
    }

    /// Construction-path write for dense array initialization.
    ///
    /// Used by parsers/builders (e.g. JSON.parse) that create fresh arrays and
    /// fill dense elements in order. This skips generic [[Set]] checks.
    #[inline]
    pub(crate) fn initialize_array_element(&self, index: usize, value: Value) {
        gc_write_barrier(&value);
        let mut elements = self.elements.borrow_mut();
        debug_assert!(
            index < elements.len(),
            "initialize_array_element expects preallocated dense storage"
        );
        if index < elements.len() {
            elements.set(index, value);
            return;
        }
        drop(elements);
        let _ = self.set_index(index, value);
    }

    /// Construction-path write for own enumerable data properties.
    ///
    /// Used by parsers/builders (e.g. JSON.parse) for fresh ordinary objects to
    /// avoid generic [[Set]] overhead while preserving normal data-property
    /// storage in shape/dictionary layouts.
    ///
    /// Contract: callers must only append unique keys for this object.
    /// (serde_json::Map already guarantees unique keys for JSON objects.)
    #[inline]
    pub(crate) fn define_data_property_for_construction(&self, key: GcRef<JsString>, value: Value) {
        gc_write_barrier(&value);

        let key = PropertyKey::String(key);

        // Fast path for construction: append a fresh property without checking
        // existing slots via shape lookup. This is valid for JSON object maps
        // where keys are unique after parsing.
        if self.flags.borrow().is_dictionary {
            if let Some(map) = self.dictionary_properties.borrow_mut().as_mut() {
                map.insert(key, PropertyDescriptor::data(value));
            }
            return;
        }

        let mut shape_write = self.shape.borrow_mut();
        let next_shape = shape_write.transition(key.clone());
        let offset = next_shape
            .offset
            .expect("Shape transition should have an offset");

        if offset >= DICTIONARY_THRESHOLD {
            drop(shape_write);
            self.transition_to_dictionary();
            if let Some(map) = self.dictionary_properties.borrow_mut().as_mut() {
                map.insert(key, PropertyDescriptor::data(value));
            }
            return;
        }

        *shape_write = next_shape;

        if offset < INLINE_PROPERTY_COUNT {
            self.inline_slots.borrow_mut()[offset] = value;
            self.inline_meta.borrow_mut()[offset] = SlotMeta::DEFAULT_DATA;
        } else {
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            let mut slots = self.overflow_slots.borrow_mut();
            let mut meta = self.overflow_meta.borrow_mut();
            if overflow_idx >= slots.len() {
                slots.resize(overflow_idx + 1, Value::undefined());
                meta.resize(overflow_idx + 1, SlotMeta::EMPTY);
            }
            slots[overflow_idx] = value;
            meta[overflow_idx] = SlotMeta::DEFAULT_DATA;
        }
    }

    pub fn array_push(&self, value: Value) -> usize {
        gc_write_barrier(&value);
        let mut elements = self.elements.borrow_mut();
        elements.push(value);
        let len = elements.len();
        self.flags.borrow_mut().dense_array_length_hint = len as u32;
        len
    }

    pub fn array_pop(&self) -> Value {
        let mut elements = self.elements.borrow_mut();
        let val = elements.pop().unwrap_or(Value::undefined());
        self.flags.borrow_mut().dense_array_length_hint = elements.len() as u32;
        if val.is_hole() {
            Value::undefined()
        } else {
            val
        }
    }

    pub fn array_shift(&self) -> Value {
        let mut elements = self.elements.borrow_mut();
        let val = elements.shift().unwrap_or(Value::undefined());
        self.flags.borrow_mut().dense_array_length_hint = elements.len() as u32;
        if val.is_hole() {
            Value::undefined()
        } else {
            val
        }
    }

    pub fn array_unshift(&self, value: Value) -> usize {
        gc_write_barrier(&value);
        let mut elements = self.elements.borrow_mut();
        elements.unshift(value);
        let len = elements.len();
        self.flags.borrow_mut().dense_array_length_hint = len as u32;
        len
    }

    pub fn array_reverse(&self) {
        let mut elements = self.elements.borrow_mut();
        elements.reverse();
    }

    pub fn array_append_all(&self, other: &JsObject) {
        let other_elements = other.elements.borrow();
        let mut elements = self.elements.borrow_mut();
        elements.append_all(&other_elements);
        let len = elements.len();
        self.flags.borrow_mut().dense_array_length_hint = len as u32;
    }

    pub fn array_splice(
        &self,
        start: usize,
        delete_count: usize,
        items: &[Value],
        _mm: &MemoryManager,
    ) -> GcRef<JsObject> {
        let mut elements = self.elements.borrow_mut();
        let deleted_kind = elements.splice(start, delete_count, items);
        let new_len = elements.len();
        self.flags.borrow_mut().dense_array_length_hint = new_len as u32;

        let deleted_obj = GcRef::new(JsObject::array(0));
        {
            let mut del_elements = deleted_obj.elements.borrow_mut();
            *del_elements = deleted_kind;
        }
        deleted_obj.flags.borrow_mut().dense_array_length_hint =
            deleted_obj.elements.borrow().len() as u32;
        deleted_obj
    }

    pub fn array_copy_within(&self, to: usize, from: usize, count: usize) {
        let mut elements = self.elements.borrow_mut();
        elements.copy_within(to, from, count);
    }

    pub fn array_sort_with_comparator<F>(&self, compare: F)
    where
        F: FnMut(&Value, &Value) -> std::cmp::Ordering,
    {
        let mut elements = self.elements.borrow_mut();
        elements.sort_with_comparator(compare);
    }

    /// Get inline slots storage (for GC tracing)
    pub(crate) fn get_inline_slots(&self) -> &ObjectCell<[Value; INLINE_PROPERTY_COUNT]> {
        &self.inline_slots
    }

    /// Get inline meta storage (for GC tracing)
    pub(crate) fn get_inline_meta(&self) -> &ObjectCell<[SlotMeta; INLINE_PROPERTY_COUNT]> {
        &self.inline_meta
    }

    /// Get overflow slots storage (for GC tracing)
    pub(crate) fn get_overflow_slots(&self) -> &ObjectCell<Vec<Value>> {
        &self.overflow_slots
    }

    /// Get overflow meta storage (for GC tracing)
    pub(crate) fn get_overflow_meta(&self) -> &ObjectCell<Vec<SlotMeta>> {
        &self.overflow_meta
    }

    pub(crate) fn get_elements_storage(&self) -> &ObjectCell<ElementsKind> {
        &self.elements
    }

    pub(crate) fn get_prototype_storage(&self) -> &ObjectCell<Value> {
        &self.prototype
    }
}

impl std::fmt::Debug for JsObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inline_count = self
            .inline_meta
            .borrow()
            .iter()
            .filter(|m| !m.is_empty())
            .count();
        let overflow_count = self
            .overflow_meta
            .borrow()
            .iter()
            .filter(|m| !m.is_empty())
            .count();
        let flags = self.flags.borrow();
        f.debug_struct("JsObject")
            .field("inline_slots", &inline_count)
            .field("overflow_slots", &overflow_count)
            .field("is_array", &flags.is_array)
            .finish()
    }
}

// SAFETY: JsObject uses ObjectCell (UnsafeCell) for interior mutability.
// Thread confinement is enforced by the Isolate abstraction: each Isolate
// is `Send` but `!Sync`, ensuring only one thread accesses its object graph
// at any time. JsObject is `Sync` because it is never actually shared between
// threads — all sharing goes through structured clone (copy semantics).
// The `Sync` impl is required for `GcRef<JsObject>` to be `Send` (per
// GcRef's bounds: `T: Send + Sync`), enabling Isolate thread migration.
unsafe impl Send for JsObject {}
unsafe impl Sync for JsObject {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_property_key_string_canonical_index_fast_path() {
        assert_eq!(PropertyKey::string("0"), PropertyKey::Index(0));
        assert_eq!(
            PropertyKey::string("4294967294"),
            PropertyKey::Index(PropertyKey::MAX_ARRAY_INDEX)
        );
    }

    #[test]
    fn test_property_key_string_non_canonical_index() {
        let _rt = crate::runtime::VmRuntime::new();
        assert!(matches!(PropertyKey::string("01"), PropertyKey::String(_)));
        assert!(matches!(PropertyKey::string("-1"), PropertyKey::String(_)));
        assert!(matches!(
            PropertyKey::string("4294967295"),
            PropertyKey::String(_)
        ));
    }

    #[test]
    fn test_property_key_from_js_string_index_fast_path() {
        let _rt = crate::runtime::VmRuntime::new();
        let js_idx = JsString::intern("123");
        let js_non_idx = JsString::intern("00123");

        assert_eq!(PropertyKey::from_js_string(js_idx), PropertyKey::Index(123));
        assert!(matches!(
            PropertyKey::from_js_string(js_non_idx),
            PropertyKey::String(_)
        ));
    }

    #[test]
    fn test_own_keys_uses_canonical_array_indices_only() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let obj = JsObject::new(Value::null());

        let _ = obj.set(PropertyKey::String(JsString::intern("2")), Value::int32(1));
        let _ = obj.set(PropertyKey::String(JsString::intern("1")), Value::int32(2));
        let _ = obj.set(PropertyKey::String(JsString::intern("01")), Value::int32(3));
        let _ = obj.set(PropertyKey::String(JsString::intern("a")), Value::int32(4));

        let keys = obj.own_keys();
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[0], PropertyKey::Index(1));
        assert_eq!(keys[1], PropertyKey::Index(2));
        assert!(matches!(keys[2], PropertyKey::String(s) if s.as_str() == "01"));
        assert!(matches!(keys[3], PropertyKey::String(s) if s.as_str() == "a"));
    }

    #[test]
    fn test_object_get_set() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let obj = JsObject::new(Value::null());

        obj.set(PropertyKey::string("foo"), Value::int32(42))
            .unwrap();
        assert_eq!(obj.get(&PropertyKey::string("foo")), Some(Value::int32(42)));
    }

    #[test]
    fn test_object_has() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let obj = JsObject::new(Value::null());
        obj.set(PropertyKey::string("foo"), Value::int32(42))
            .unwrap();

        assert!(obj.has(&PropertyKey::string("foo")));
        assert!(!obj.has(&PropertyKey::string("bar")));
    }

    #[test]
    fn test_array() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let arr = JsObject::array(3);
        assert!(arr.is_array());
        assert_eq!(arr.array_length(), 3);

        arr.set(PropertyKey::Index(0), Value::int32(1)).unwrap();
        arr.set(PropertyKey::Index(1), Value::int32(2)).unwrap();
        arr.set(PropertyKey::Index(2), Value::int32(3)).unwrap();

        assert_eq!(arr.get(&PropertyKey::Index(0)), Some(Value::int32(1)));
        assert_eq!(arr.get(&PropertyKey::Index(1)), Some(Value::int32(2)));
        assert_eq!(arr.get(&PropertyKey::Index(2)), Some(Value::int32(3)));
    }

    #[test]
    fn test_array_holes() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let arr = JsObject::array(3);
        arr.set(PropertyKey::Index(0), Value::int32(10)).unwrap();
        arr.set(PropertyKey::Index(1), Value::int32(20)).unwrap();
        arr.set(PropertyKey::Index(2), Value::int32(30)).unwrap();

        // Delete creates a hole
        assert!(arr.delete(&PropertyKey::Index(1)));

        // has_own returns false for holes
        assert!(arr.has_own(&PropertyKey::Index(0)));
        assert!(!arr.has_own(&PropertyKey::Index(1)));
        assert!(arr.has_own(&PropertyKey::Index(2)));

        // get returns None for holes
        assert_eq!(arr.get(&PropertyKey::Index(0)), Some(Value::int32(10)));
        assert_eq!(arr.get(&PropertyKey::Index(1)), None);
        assert_eq!(arr.get(&PropertyKey::Index(2)), Some(Value::int32(30)));

        // own_keys skips holes
        let keys = arr.own_keys();
        assert!(keys.contains(&PropertyKey::Index(0)));
        assert!(!keys.contains(&PropertyKey::Index(1)));
        assert!(keys.contains(&PropertyKey::Index(2)));
    }

    #[test]
    fn test_array_prefill_holes() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let arr = JsObject::array(5);
        // new Array(5) should create holes, not present elements
        assert!(!arr.has_own(&PropertyKey::Index(0)));
        assert!(!arr.has_own(&PropertyKey::Index(4)));
        assert_eq!(arr.array_length(), 5);
    }

    #[test]
    fn test_array_length_truncate() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let arr = JsObject::array(0);
        arr.set(PropertyKey::Index(0), Value::int32(1)).unwrap();
        arr.set(PropertyKey::Index(1), Value::int32(2)).unwrap();
        arr.set(PropertyKey::Index(2), Value::int32(3)).unwrap();
        arr.set(PropertyKey::Index(3), Value::int32(4)).unwrap();
        arr.set(PropertyKey::Index(4), Value::int32(5)).unwrap();
        assert_eq!(arr.array_length(), 5);

        // arr.length = 2 should truncate
        arr.set_array_length(2);
        assert_eq!(arr.array_length(), 2);
        assert_eq!(arr.get(&PropertyKey::Index(0)), Some(Value::int32(1)));
        assert_eq!(arr.get(&PropertyKey::Index(1)), Some(Value::int32(2)));
        assert_eq!(arr.get(&PropertyKey::Index(2)), None);

        // arr.length = 10 should extend with holes
        arr.set_array_length(10);
        assert_eq!(arr.array_length(), 10);
        assert!(!arr.has_own(&PropertyKey::Index(5)));
    }

    #[test]
    fn test_array_length_set_via_property() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let arr = JsObject::array(0);
        arr.set(PropertyKey::Index(0), Value::int32(10)).unwrap();
        arr.set(PropertyKey::Index(1), Value::int32(20)).unwrap();
        arr.set(PropertyKey::Index(2), Value::int32(30)).unwrap();

        // Setting length via set("length", 1) should truncate
        arr.set(PropertyKey::string("length"), Value::number(1.0))
            .unwrap();
        assert_eq!(arr.array_length(), 1);
        assert_eq!(arr.get(&PropertyKey::Index(0)), Some(Value::int32(10)));
        assert_eq!(arr.get(&PropertyKey::Index(1)), None);
    }

    #[test]
    fn test_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JsObject>();
    }

    #[test]
    fn test_object_freeze() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let obj = JsObject::new(Value::null());
        obj.set(PropertyKey::string("foo"), Value::int32(42))
            .unwrap();

        assert!(!obj.is_frozen());
        assert!(obj.is_extensible());

        obj.freeze();

        assert!(obj.is_frozen());
        assert!(obj.is_sealed());
        assert!(!obj.is_extensible());

        // Cannot modify existing property
        assert!(
            obj.set(PropertyKey::string("foo"), Value::int32(100))
                .is_err()
        );
        assert_eq!(obj.get(&PropertyKey::string("foo")), Some(Value::int32(42)));

        // Cannot add new property
        assert!(
            obj.set(PropertyKey::string("bar"), Value::int32(200))
                .is_err()
        );
        assert_eq!(obj.get(&PropertyKey::string("bar")), None);

        // Cannot delete property
        assert!(!obj.delete(&PropertyKey::string("foo")));
        assert!(obj.has_own(&PropertyKey::string("foo")));
    }

    #[test]
    fn test_object_seal() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let obj = JsObject::new(Value::null());
        let _ = obj.set(PropertyKey::string("foo"), Value::int32(42));

        assert!(!obj.is_sealed());

        obj.seal();

        assert!(obj.is_sealed());
        assert!(!obj.is_frozen());
        assert!(!obj.is_extensible());

        // CAN modify existing property (seal allows writes, freeze doesn't)
        assert!(
            obj.set(PropertyKey::string("foo"), Value::int32(100))
                .is_ok()
        );
        assert_eq!(
            obj.get(&PropertyKey::string("foo")),
            Some(Value::int32(100))
        );

        // Cannot add new property
        assert!(
            obj.set(PropertyKey::string("bar"), Value::int32(200))
                .is_err()
        );
        assert_eq!(obj.get(&PropertyKey::string("bar")), None);

        // Cannot delete property
        assert!(!obj.delete(&PropertyKey::string("foo")));
    }

    #[test]
    fn test_object_prevent_extensions() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let obj = JsObject::new(Value::null());
        let _ = obj.set(PropertyKey::string("foo"), Value::int32(42));

        assert!(obj.is_extensible());

        obj.prevent_extensions();

        assert!(!obj.is_extensible());
        assert!(!obj.is_sealed());
        assert!(!obj.is_frozen());

        // CAN modify existing property
        assert!(
            obj.set(PropertyKey::string("foo"), Value::int32(100))
                .is_ok()
        );

        // Cannot add new property
        assert!(
            obj.set(PropertyKey::string("bar"), Value::int32(200))
                .is_err()
        );

        // Can delete existing (configurable) property even if not extensible
        assert!(obj.delete(&PropertyKey::string("foo")));
        assert_eq!(obj.get(&PropertyKey::string("foo")), None);
    }

    #[test]
    fn test_deep_prototype_chain() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        // Build a prototype chain of depth 100
        let mut proto_val = Value::null();
        for i in 0..100 {
            let obj = GcRef::new(JsObject::new(proto_val));
            let _ = obj.set(
                PropertyKey::string(&format!("prop{}", i)),
                Value::int32(i as i32),
            );
            proto_val = Value::object(obj);
        }

        let child = JsObject::new(proto_val);

        // Should be able to access properties at depth 100
        assert_eq!(
            child.get(&PropertyKey::string("prop0")),
            Some(Value::int32(0))
        );
        assert_eq!(
            child.get(&PropertyKey::string("prop99")),
            Some(Value::int32(99))
        );
        assert!(child.has(&PropertyKey::string("prop50")));
    }

    #[test]
    fn test_prototype_chain_depth_limit() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        // Build a prototype chain that exceeds the limit (100)
        let mut proto_val = Value::null();
        for i in 0..110 {
            let obj = GcRef::new(JsObject::new(proto_val));
            if i == 0 {
                let _ = obj.set(PropertyKey::string("deep_prop"), Value::int32(42));
            }
            proto_val = Value::object(obj);
        }

        let child = JsObject::new(proto_val);

        // Property at depth > 100 should not be found (returns None gracefully)
        assert_eq!(child.get(&PropertyKey::string("deep_prop")), None);
        assert!(!child.has(&PropertyKey::string("deep_prop")));
    }

    #[test]
    fn test_prototype_cycle_prevention() {
        let _rt = crate::runtime::VmRuntime::new();
        let _memory_manager = _rt.memory_manager().clone();
        let obj1 = GcRef::new(JsObject::new(Value::null()));
        let obj2 = GcRef::new(JsObject::new(Value::object(obj1)));
        let obj3 = GcRef::new(JsObject::new(Value::object(obj2)));

        // Attempting to create a cycle should fail
        // obj1 -> obj2 -> obj3 -> obj1 would be a cycle
        assert!(!obj1.set_prototype(Value::object(obj3)));

        // Setting to null should work
        assert!(obj1.set_prototype(Value::null()));

        // Setting to an unrelated object should work
        let unrelated = GcRef::new(JsObject::new(Value::null()));
        assert!(obj1.set_prototype(Value::object(unrelated)));
    }
}

// ============================================================================
// GC Tracing Implementation
// ============================================================================

impl otter_vm_gc::GcTraceable for JsObject {
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::OBJECT;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace prototype (now a Value)
        self.prototype.borrow().trace(tracer);

        // Trace shape property keys.
        //
        // Shapes are Arc-managed (not GC-managed), but they hold GcRef<JsString>
        // and GcRef<Symbol> as property key identifiers.  Without this call the
        // GC sees those strings/symbols as unreachable and collects them, turning
        // every subsequent property-name lookup into a use-after-free.
        self.shape.borrow().trace_keys(tracer);

        // Trace values in inline slots (data values and accessor pair GcRefs)
        {
            let slots = self.inline_slots.borrow();
            let meta = self.inline_meta.borrow();
            for i in 0..INLINE_PROPERTY_COUNT {
                if !meta[i].is_empty() {
                    slots[i].trace(tracer);
                }
            }
        }

        // Trace values in overflow slots
        {
            let slots = self.overflow_slots.borrow();
            let meta = self.overflow_meta.borrow();
            for i in 0..slots.len() {
                if !meta[i].is_empty() {
                    slots[i].trace(tracer);
                }
            }
        }

        // Trace keys AND values in dictionary properties.
        // Keys are GcRef<JsString>/GcRef<Symbol> just like shape keys and must
        // be kept alive for the same reason.
        if let Some(dict) = self.dictionary_properties.borrow().as_ref() {
            for (key, desc) in dict.iter() {
                match key {
                    PropertyKey::String(s) => tracer(s.header() as *const _),
                    PropertyKey::Symbol(sym) => tracer(sym.header() as *const _),
                    PropertyKey::Index(_) => {}
                }
                desc.trace(tracer);
            }
        }

        // Trace array elements
        for value in self.elements.borrow().iter() {
            value.trace(tracer);
        }

        // Trace argument mapping upvalue cells (sloppy mode mapped arguments)
        if let Some(mapping) = self.argument_mapping.borrow().as_ref() {
            for cell_opt in &mapping.cells {
                if let Some(cell) = cell_opt {
                    cell.get().trace(tracer);
                }
            }
        }
    }
}

impl PropertyDescriptor {
    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        match self {
            PropertyDescriptor::Data { value, .. } => {
                value.trace(tracer);
            }
            PropertyDescriptor::Accessor { get, set, .. } => {
                if let Some(getter) = get {
                    getter.trace(tracer);
                }
                if let Some(setter) = set {
                    setter.trace(tracer);
                }
            }
            PropertyDescriptor::Deleted => {}
        }
    }
}
