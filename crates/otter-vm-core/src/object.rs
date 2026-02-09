//! JavaScript objects with hidden classes (shapes)
//!
//! Objects use hidden classes (called "shapes") for property access optimization.
//! This is similar to V8's approach.
//!
//! ## Inline Properties (JSC Pattern)
//!
//! The first few properties (up to `INLINE_PROPERTY_COUNT`) are stored inline
//! in the object struct rather than in a separate Vec. This improves cache
//! locality and reduces indirection for common cases where objects have few
//! properties.

use crate::object_cell::ObjectCell;
use indexmap::IndexMap;
use std::sync::Arc;

use crate::gc::GcRef;
use crate::shape::Shape;
use crate::value::UpvalueCell;

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

/// Number of properties stored inline in the object (JSC-style optimization)
/// Properties beyond this count overflow to a Vec.
pub const INLINE_PROPERTY_COUNT: usize = 4;

/// Threshold for transitioning to dictionary mode.
/// Objects with more than this many properties, or objects that have had
/// properties deleted, switch to HashMap-based storage for better memory
/// efficiency at the cost of IC cacheability.
pub const DICTIONARY_THRESHOLD: usize = 32;
use crate::string::JsString;
use crate::value::Value;

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
    /// Maximum valid array index per ECMA-262: 0 .. 2^32 - 2.
    /// The value 2^32 - 1 (4294967295) is NOT a valid array index.
    pub const MAX_ARRAY_INDEX: u32 = u32::MAX - 1; // 4294967294

    /// Create a string property key (canonicalizes numeric strings to Index)
    pub fn string(s: &str) -> Self {
        // Canonicalize numeric strings to Index for consistent lookup.
        // Only values 0..=MAX_ARRAY_INDEX are valid array indices per spec.
        if let Ok(n) = s.parse::<u32>() {
            if n <= Self::MAX_ARRAY_INDEX && n.to_string() == s {
                return Self::Index(n);
            }
        }
        let js_str = JsString::intern(s);
        Self::String(js_str)
    }

    /// Create from a GcRef<JsString>
    pub fn from_js_string(s: GcRef<JsString>) -> Self {
        // Canonicalize numeric strings to Index for consistent lookup
        let str_val = s.as_str();
        if let Ok(n) = str_val.parse::<u32>() {
            if n <= Self::MAX_ARRAY_INDEX && n.to_string() == str_val {
                return Self::Index(n);
            }
        }
        Self::String(s)
    }

    /// Create an index property key
    pub fn index(i: u32) -> Self {
        Self::Index(i)
    }

    /// Trace property key for GC
    pub fn trace(&self, tracer: &mut dyn crate::gc::Tracer) {
        match self {
            Self::String(s) => {
                // GcRef provides header() via GcBox wrapper
                tracer.mark_header(s.header() as *const _);
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
            attributes: PropertyAttributes::accessor(),
        }
    }
}

/// Internal property storage entry
#[derive(Clone, Debug)]
pub(crate) struct PropertyEntry {
    /// Descriptor for the property (Data or Accessor)
    pub(crate) desc: PropertyDescriptor,
}

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
}

impl std::fmt::Display for SetPropertyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frozen => write!(f, "Cannot assign to property of a frozen object"),
            Self::NonWritable => write!(f, "Cannot assign to read only property"),
            Self::NonExtensible => write!(f, "Cannot add property, object is not extensible"),
            Self::Sealed => write!(f, "Cannot add property to a sealed object"),
            Self::AccessorWithoutSetter => write!(f, "Cannot set property which has only a getter"),
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
pub(crate) fn get_value_full(
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
/// Both inline and overflow use `PropertyEntry` to support accessor properties.
pub struct JsObject {
    /// Current shape of the object
    shape: ObjectCell<Arc<Shape>>,
    /// Inline property storage for first N properties (JSC-style)
    inline_properties: ObjectCell<[Option<PropertyEntry>; INLINE_PROPERTY_COUNT]>,
    /// Overflow properties storage (for properties beyond INLINE_PROPERTY_COUNT)
    overflow_properties: ObjectCell<Vec<PropertyEntry>>,
    /// Dictionary mode property storage (used when is_dictionary flag is set)
    /// When in dictionary mode, shape/inline/overflow are ignored for property access.
    dictionary_properties: ObjectCell<Option<IndexMap<PropertyKey, PropertyEntry>>>,
    /// Prototype (null for Object.prototype, mutable via Reflect.setPrototypeOf)
    /// Can be Value::object, Value::proxy, or Value::null
    prototype: ObjectCell<Value>,
    /// Array elements (for array-like objects)
    pub elements: ObjectCell<Vec<Value>>,
    /// Object flags (mutable for freeze/seal/preventExtensions)
    pub flags: ObjectCell<ObjectFlags>,
    /// Memory manager for accounting
    memory_manager: Arc<crate::memory::MemoryManager>,
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
    /// Whether array length is writable (None = true, default)
    pub array_length_writable: Option<bool>,
    /// String exotic object (new String("...")) — character indices are non-writable/non-configurable
    pub is_string_exotic: bool,
}

impl JsObject {
    /// Create a new empty object (prototype can be object, proxy, or null)
    pub fn new(prototype: Value, memory_manager: Arc<crate::memory::MemoryManager>) -> Self {
        // Assume basic object size for now
        let size = std::mem::size_of::<Self>();
        let _ = memory_manager.alloc(size); // ignore err in basic constructor for now or return Result

        Self {
            shape: ObjectCell::new(Shape::root()),
            inline_properties: ObjectCell::new([None, None, None, None]),
            overflow_properties: ObjectCell::new(Vec::new()),
            dictionary_properties: ObjectCell::new(None),
            prototype: ObjectCell::new(prototype),
            elements: ObjectCell::new(Vec::new()),
            flags: ObjectCell::new(ObjectFlags {
                extensible: true,
                ..Default::default()
            }),
            memory_manager,
            argument_mapping: ObjectCell::new(None),
        }
    }

    /// Set up String exotic object: populate elements with characters and set flag.
    /// ES §10.4.3: String exotic objects expose character-index properties.
    pub fn setup_string_exotic(&self, s: &str) {
        let mut elements = self.elements.borrow_mut();
        elements.clear();
        for ch in s.chars() {
            let mut buf = [0u8; 4];
            let cs = ch.encode_utf8(&mut buf);
            elements.push(Value::string(JsString::intern(cs)));
        }
        drop(elements);
        self.flags.borrow_mut().is_string_exotic = true;
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

    /// Get argument mapping for GC tracing
    pub fn argument_mapping_cells(&self) -> Vec<UpvalueCell> {
        let mapping = self.argument_mapping.borrow();
        match mapping.as_ref() {
            Some(m) => m.cells.iter().filter_map(|c| c.clone()).collect(),
            None => Vec::new(),
        }
    }

    pub fn memory_manager(&self) -> &Arc<crate::memory::MemoryManager> {
        &self.memory_manager
    }

    /// Create a new array
    pub fn array(length: usize, memory_manager: Arc<crate::memory::MemoryManager>) -> Self {
        let obj = Self::new(Value::null(), memory_manager);
        // Cap dense element pre-allocation to avoid OOM on large sparse arrays.
        const MAX_DENSE_PREALLOC: usize = 1 << 24; // 16M elements
        let mut flags = obj.flags.borrow_mut();
        flags.is_array = true;
        if length <= MAX_DENSE_PREALLOC {
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
    pub fn array_like(length: usize, memory_manager: Arc<crate::memory::MemoryManager>) -> Self {
        let obj = Self::new(Value::null(), memory_manager);
        const MAX_DENSE_PREALLOC: usize = 1 << 24;
        if length <= MAX_DENSE_PREALLOC {
            obj.elements.borrow_mut().resize(length, Value::undefined());
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
            let inline = self.inline_properties.borrow();
            inline[offset]
                .as_ref()
                .and_then(|e| e.desc.value().cloned())
        } else {
            let overflow = self.overflow_properties.borrow();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            overflow
                .get(overflow_idx)
                .and_then(|e| e.desc.value().cloned())
        }
    }

    /// Get property entry by offset (includes accessor properties)
    #[inline]
    pub fn get_property_entry_by_offset(&self, offset: usize) -> Option<PropertyDescriptor> {
        let desc = if offset < INLINE_PROPERTY_COUNT {
            let inline = self.inline_properties.borrow();
            inline[offset].as_ref().map(|e| e.desc.clone())?
        } else {
            let overflow = self.overflow_properties.borrow();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            overflow.get(overflow_idx).map(|e| e.desc.clone())?
        };

        match desc {
            PropertyDescriptor::Deleted => None,
            other => Some(other),
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
            let mut inline = self.inline_properties.borrow_mut();
            if let Some(entry) = inline[offset].as_mut() {
                match &mut entry.desc {
                    PropertyDescriptor::Deleted => {
                        if !is_extensible {
                            return Err(SetPropertyError::NonExtensible);
                        }
                        if is_sealed {
                            return Err(SetPropertyError::Sealed);
                        }
                        entry.desc = PropertyDescriptor::data(value);
                        return Ok(());
                    }
                    PropertyDescriptor::Data {
                        value: v,
                        attributes,
                    } => {
                        if attributes.writable {
                            *v = value;
                            return Ok(());
                        }
                        return Err(SetPropertyError::NonWritable);
                    }
                    PropertyDescriptor::Accessor { .. } => {
                        return Err(SetPropertyError::AccessorWithoutSetter);
                    }
                }
            }
            Err(SetPropertyError::NonExtensible)
        } else {
            let mut overflow = self.overflow_properties.borrow_mut();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            if let Some(entry) = overflow.get_mut(overflow_idx) {
                match &mut entry.desc {
                    PropertyDescriptor::Deleted => {
                        if !is_extensible {
                            return Err(SetPropertyError::NonExtensible);
                        }
                        if is_sealed {
                            return Err(SetPropertyError::Sealed);
                        }
                        entry.desc = PropertyDescriptor::data(value);
                        return Ok(());
                    }
                    PropertyDescriptor::Data {
                        value: v,
                        attributes,
                    } => {
                        if attributes.writable {
                            *v = value;
                            return Ok(());
                        }
                        return Err(SetPropertyError::NonWritable);
                    }
                    PropertyDescriptor::Accessor { .. } => {
                        return Err(SetPropertyError::AccessorWithoutSetter);
                    }
                }
            }
            Err(SetPropertyError::NonExtensible)
        }
    }

    /// Get total property count (inline + overflow)
    #[allow(dead_code)]
    fn property_count(&self) -> usize {
        let inline = self.inline_properties.borrow();
        let inline_count = inline.iter().filter(|v| v.is_some()).count();
        let overflow = self.overflow_properties.borrow();
        inline_count + overflow.len()
    }

    /// Get current shape
    pub fn shape(&self) -> Arc<Shape> {
        self.shape.borrow().clone()
    }

    /// Check if object is in dictionary mode (IC-uncacheable).
    /// Objects in dictionary mode use HashMap storage instead of shape-based indexed storage.
    #[inline]
    pub fn is_dictionary_mode(&self) -> bool {
        self.flags.borrow().is_dictionary
    }

    /// Debug: get the number of keys in the shape
    pub fn get_shape_key_count(&self) -> usize {
        self.shape.borrow().own_keys().len()
    }

    /// Debug: get number of non-None inline property slots
    pub fn get_inline_occupied_count(&self) -> usize {
        self.inline_properties
            .borrow()
            .iter()
            .filter(|e| e.is_some())
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

        // Build HashMap from existing properties
        let mut dict = IndexMap::new();

        let shape = self.shape.borrow();
        let inline = self.inline_properties.borrow();
        let overflow = self.overflow_properties.borrow();

        // Iterate over all properties in the shape
        // IMPORTANT: Use the actual offset from shape, not a sequential counter
        for key in shape.own_keys() {
            if let Some(offset) = shape.get_offset(&key) {
                let entry = if offset < INLINE_PROPERTY_COUNT {
                    inline[offset].clone()
                } else {
                    overflow.get(offset - INLINE_PROPERTY_COUNT).cloned()
                };

                if let Some(entry) = entry {
                    // Skip deleted entries
                    if !matches!(entry.desc, PropertyDescriptor::Deleted) {
                        dict.insert(key, entry);
                    }
                }
            }
        }

        drop(shape);
        drop(inline);
        drop(overflow);

        // Store the dictionary
        *self.dictionary_properties.borrow_mut() = Some(dict);
        // Replace shape with a fresh root — the unique Arc pointer invalidates
        // all IC entries that cached the old shape_id for this object.
        *self.shape.borrow_mut() = Shape::root();
        flags.is_dictionary = true;
    }

    /// Get property by key
    pub fn get(&self, key: &PropertyKey) -> Option<Value> {
        // Special handling for array "length" property
        if self.is_array()
            && let PropertyKey::String(s) = key
            && s.as_str() == "length"
        {
            let flags = self.flags.borrow();
            if let Some(sparse_len) = flags.sparse_array_length {
                return Some(Value::number(sparse_len as f64));
            }
            drop(flags);
            return Some(Value::int32(self.elements.borrow().len() as i32));
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
                    return Some(elements[idx].clone());
                }
                return None;
            }
        }

        // Dictionary mode: use HashMap lookup
        if self.is_dictionary_mode() {
            if let Some(dict) = self.dictionary_properties.borrow().as_ref() {
                if let Some(entry) = dict.get(key) {
                    match &entry.desc {
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
                let val = &elements[idx];
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
                        if let Some(entry) = dict.get(key) {
                            match &entry.desc {
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

        // Clear inline properties
        {
            let mut inline = self.inline_properties.borrow_mut();
            for slot in inline.iter_mut() {
                if let Some(entry) = slot.take() {
                    match entry.desc {
                        PropertyDescriptor::Data { value, .. } => values.push(value),
                        PropertyDescriptor::Accessor { get, set, .. } => {
                            if let Some(v) = get {
                                values.push(v);
                            }
                            if let Some(v) = set {
                                values.push(v);
                            }
                        }
                        PropertyDescriptor::Deleted => {}
                    }
                }
            }
        }

        // Clear overflow properties
        {
            let mut overflow = self.overflow_properties.borrow_mut();
            for entry in overflow.drain(..) {
                match entry.desc {
                    PropertyDescriptor::Data { value, .. } => values.push(value),
                    PropertyDescriptor::Accessor { get, set, .. } => {
                        if let Some(v) = get {
                            values.push(v);
                        }
                        if let Some(v) = set {
                            values.push(v);
                        }
                    }
                    PropertyDescriptor::Deleted => {}
                }
            }
        }

        // Clear elements
        {
            let mut elems = self.elements.borrow_mut();
            for val in elems.drain(..) {
                values.push(val);
            }
        }

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
                    return Some(PropertyDescriptor::Data {
                        value: Value::number(self.array_length() as f64),
                        attributes: PropertyAttributes {
                            writable: self.flags.borrow().array_length_writable.unwrap_or(true),
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
                        value: elements[idx].clone(),
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
                if let Some(e) = dict.get(key) {
                    return Some(e.desc.clone());
                }
                // For Index keys, also try as String (e.g., Index(2) -> String("2"))
                if let PropertyKey::Index(i) = key {
                    let str_key = PropertyKey::string(&i.to_string());
                    if let Some(e) = dict.get(&str_key) {
                        return Some(e.desc.clone());
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
            if idx < elements.len() && !elements[idx].is_hole() {
                return Some(PropertyDescriptor::data(elements[idx].clone()));
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
                    if idx < elements.len() && !elements[idx].is_hole() {
                        return Some(PropertyDescriptor::data(elements[idx].clone()));
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

        // Frozen objects cannot have properties changed
        if flags.frozen {
            return Err(SetPropertyError::Frozen);
        }

        // Array exotic: intercept `length` writes to truncate/extend
        if flags.is_array {
            if let PropertyKey::String(s) = &key {
                if s.as_str() == "length" {
                    drop(flags);
                    let new_len = value.as_number().unwrap_or(0.0);
                    if new_len < 0.0 || new_len != (new_len as u32 as f64) || new_len.is_nan() {
                        return Err(SetPropertyError::NonExtensible); // RangeError in spec
                    }
                    if self.set_array_length(new_len as u32) {
                        return Ok(());
                    }
                    return Err(SetPropertyError::NonExtensible);
                }
            }
        }

        // String exotic objects: character indices are non-writable
        if flags.is_string_exotic {
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
                cell.set(value.clone());
                // Also update elements for when mapping is later removed
                let mut elements = self.elements.borrow_mut();
                if idx < elements.len() {
                    elements[idx] = value;
                }
                return Ok(());
            }
            let mut elements = self.elements.borrow_mut();
            if idx < elements.len() {
                elements[idx] = value;
                return Ok(());
            } else if flags.is_array && flags.extensible && !flags.sealed {
                // Cap dense element storage to avoid OOM on sparse arrays.
                // Indices beyond this limit are stored as dictionary properties.
                const MAX_DENSE_LENGTH: usize = 1 << 24; // 16M elements
                if idx < MAX_DENSE_LENGTH {
                    elements.resize(idx + 1, Value::hole());
                    elements[idx] = value;
                    return Ok(());
                }
                // Large sparse index — fall through to dictionary/string storage
            }
            drop(elements);
            drop(flags);
            // For non-arrays, fall through to store as string property
            let string_key = PropertyKey::String(crate::string::JsString::intern(&i.to_string()));
            return self.set(string_key, value);
        }

        // Dictionary mode: use HashMap storage
        if flags.is_dictionary {
            drop(flags);
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                // If the property already exists, check writability and preserve attributes
                if let Some(existing) = map.get(&key) {
                    match &existing.desc {
                        PropertyDescriptor::Data { attributes, .. } => {
                            if !attributes.writable {
                                return Err(SetPropertyError::NonWritable);
                            }
                            // Preserve existing attributes, only update value
                            let attrs = *attributes;
                            map.insert(
                                key,
                                PropertyEntry {
                                    desc: PropertyDescriptor::Data {
                                        value,
                                        attributes: attrs,
                                    },
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
                let entry = PropertyEntry {
                    desc: PropertyDescriptor::data(value),
                };
                map.insert(key, entry);
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
                drop(flags);
                return self.set_by_offset(offset, value);
            }
        }

        // New property addition
        if flags.extensible && !flags.sealed {
            drop(flags);

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
                    let entry = PropertyEntry {
                        desc: PropertyDescriptor::data(value),
                    };
                    // Re-insert the key we were adding
                    map.insert(next_shape.key.clone().unwrap(), entry);
                    return Ok(());
                }
                return Err(SetPropertyError::NonExtensible);
            }

            *shape_write = next_shape;

            let entry = PropertyEntry {
                desc: PropertyDescriptor::data(value),
            };

            if offset < INLINE_PROPERTY_COUNT {
                // Store in inline slot
                let mut inline = self.inline_properties.borrow_mut();
                inline[offset] = Some(entry);
            } else {
                // Store in overflow
                let mut overflow = self.overflow_properties.borrow_mut();
                let overflow_idx = offset - INLINE_PROPERTY_COUNT;
                if overflow_idx >= overflow.len() {
                    overflow.resize(
                        overflow_idx + 1,
                        PropertyEntry {
                            desc: PropertyDescriptor::Deleted,
                        },
                    );
                }
                overflow[overflow_idx] = entry;
            }
            Ok(())
        } else if !flags.extensible {
            Err(SetPropertyError::NonExtensible)
        } else {
            Err(SetPropertyError::Sealed)
        }
    }

    /// Delete property
    pub fn delete(&self, key: &PropertyKey) -> bool {
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

            // ES §10.4.2.1 [[Delete]](P): Deleting an indexed property creates
            // a hole but NEVER changes the array's length. We set the element to
            // hole, then compact trailing holes to save memory, but preserve the
            // original length via sparse_array_length so array_length() stays
            // correct.
            let idx = *i as usize;
            let mut elements = self.elements.borrow_mut();
            if idx < elements.len() {
                let original_len = elements.len();
                elements[idx] = Value::hole();
                // Trim trailing holes to keep elements vec compact
                while elements.last().map_or(false, |v| v.is_hole()) {
                    elements.pop();
                }
                let new_len = elements.len();
                drop(elements); // Must drop before calling array_length()
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
                if let Some(entry) = map.get(key) {
                    if !entry.desc.is_configurable() {
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

            // Transition to dictionary mode on delete (creates sparse storage)
            self.transition_to_dictionary();

            // Now remove from dictionary
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                map.shift_remove(key);
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
            return idx < elements.len() && !elements[idx].is_hole();
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
                            // Check if it's a valid array index string
                            if let Ok(n) = s.as_str().parse::<u32>() {
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
                            // Check if it's a valid array index string
                            if let Ok(n) = s.as_str().parse::<u32>() {
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
            if !elements[i].is_hole() && !integer_keys.contains(&(i as u32)) {
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
                    if Self::validate_property_descriptor_change(&existing.desc, &desc).is_err() {
                        return false;
                    }
                } else if !self.flags.borrow().extensible {
                    // Cannot add new property to non-extensible object
                    return false;
                }
                map.insert(key, PropertyEntry { desc });
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
            if off < INLINE_PROPERTY_COUNT {
                let mut inline = self.inline_properties.borrow_mut();
                if let Some(entry) = inline[off].as_mut() {
                    // Validate the property descriptor change
                    if Self::validate_property_descriptor_change(&entry.desc, &desc).is_err() {
                        return false;
                    }
                    entry.desc = desc;
                    return true;
                }
            } else {
                let mut overflow = self.overflow_properties.borrow_mut();
                let overflow_idx = off - INLINE_PROPERTY_COUNT;
                if let Some(entry) = overflow.get_mut(overflow_idx) {
                    // Validate the property descriptor change
                    if Self::validate_property_descriptor_change(&entry.desc, &desc).is_err() {
                        return false;
                    }
                    entry.desc = desc;
                    return true;
                }
            }
            return false;
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
                map.insert(key, PropertyEntry { desc });
                return true;
            }
            return false;
        }

        *shape_write = next_shape;

        let entry = PropertyEntry { desc };

        if offset < INLINE_PROPERTY_COUNT {
            // Store in inline slot
            let mut inline = self.inline_properties.borrow_mut();
            inline[offset] = Some(entry);
        } else {
            // Store in overflow
            let mut overflow = self.overflow_properties.borrow_mut();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            if overflow_idx >= overflow.len() {
                overflow.resize(
                    overflow_idx + 1,
                    PropertyEntry {
                        desc: PropertyDescriptor::Deleted,
                    },
                );
            }
            overflow[overflow_idx] = entry;
        }
        true
    }

    /// Store a property without validation checks.
    /// Used by `define_own_property` after it has already validated the operation.
    fn store_property(&self, key: PropertyKey, desc: PropertyDescriptor) {
        // Dictionary mode: store directly in HashMap
        if self.flags.borrow().is_dictionary {
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                map.insert(key, PropertyEntry { desc });
            }
            return;
        }

        let offset = self.shape.borrow().get_offset(&key);

        if let Some(off) = offset {
            // Update existing slot
            let entry = PropertyEntry { desc };
            if off < INLINE_PROPERTY_COUNT {
                let mut inline = self.inline_properties.borrow_mut();
                inline[off] = Some(entry);
            } else {
                let mut overflow = self.overflow_properties.borrow_mut();
                let overflow_idx = off - INLINE_PROPERTY_COUNT;
                if overflow_idx < overflow.len() {
                    overflow[overflow_idx] = entry;
                }
            }
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
            let mut dict = self.dictionary_properties.borrow_mut();
            if let Some(map) = dict.as_mut() {
                map.insert(key, PropertyEntry { desc });
            }
            return;
        }

        *shape_write = next_shape;
        let entry = PropertyEntry { desc };
        if offset < INLINE_PROPERTY_COUNT {
            let mut inline = self.inline_properties.borrow_mut();
            inline[offset] = Some(entry);
        } else {
            let mut overflow = self.overflow_properties.borrow_mut();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            if overflow_idx >= overflow.len() {
                overflow.resize(
                    overflow_idx + 1,
                    PropertyEntry {
                        desc: PropertyDescriptor::Deleted,
                    },
                );
            }
            overflow[overflow_idx] = entry;
        }
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
                let current_is_data = matches!(existing, PropertyDescriptor::Data { .. });
                let desc_is_data = desc.is_data_descriptor();
                let desc_is_accessor = desc.is_accessor_descriptor();

                if current_is_data != desc_is_data && current_is_data != !desc_is_accessor {
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
                if desc_is_data && current_is_data {
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

                // Step 8: Both accessor descriptors
                if desc_is_accessor && matches!(existing, PropertyDescriptor::Accessor { .. }) {
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
                    return true;
                }

                // Type conversion needed (data → accessor or accessor → data)
                if !current_configurable {
                    return false;
                }
                let merged = Self::merge_partial_with_current_converting(existing, desc);
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
            PropertyKey::String(s) => {
                let str = s.as_str();
                if let Ok(n) = str.parse::<u32>() {
                    // Must be canonical: no leading zeros, and in range
                    if n <= 0xFFFF_FFFE && n.to_string() == str {
                        Some(n)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
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
            return self.ordinary_define_own_property(key, desc);
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
            if !self.elements.borrow()[idx].is_hole() {
                // Existing element with default attributes
                // Check if desc specifies non-default attributes
                if desc.has_non_default_data_attributes() {
                    // Need to store in shape for custom attributes
                    return self.ordinary_define_own_property(key, desc);
                }
                self.elements.borrow_mut()[idx] = value;
            } else {
                // Hole - create new element
                if !self.flags.borrow().extensible {
                    return false;
                }
                if desc.has_non_default_data_attributes() {
                    return self.ordinary_define_own_property(key, desc);
                }
                self.elements.borrow_mut()[idx] = value;
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
            elements.push(value);
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
                let desc_is_accessor = desc.is_accessor_descriptor();
                if current_is_data != desc_is_data && current_is_data != !desc_is_accessor {
                    if !current_configurable {
                        return false;
                    }
                    let merged = Self::merge_partial_with_current_converting(existing, desc);
                    self.store_property(key, merged);
                    return true;
                }
                if desc_is_data && current_is_data {
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
                if desc_is_accessor && matches!(existing, PropertyDescriptor::Accessor { .. }) {
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
                    return true;
                }
                if !current_configurable {
                    return false;
                }
                let merged = Self::merge_partial_with_current_converting(existing, desc);
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

        *self.prototype.borrow_mut() = prototype;
        // Bump global proto epoch to invalidate any cached prototype chain lookups
        bump_proto_epoch();
        true
    }

    /// Check if object is an array
    pub fn is_array(&self) -> bool {
        self.flags.borrow().is_array
    }

    /// Mark this object as an array exotic object
    /// Used for Array.prototype per ES2026 §23.1.3
    pub fn mark_as_array(&self) {
        self.flags.borrow_mut().is_array = true;
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
        drop(flags);

        // Make all inline properties non-writable and non-configurable
        let mut inline = self.inline_properties.borrow_mut();
        for entry_opt in inline.iter_mut() {
            if let Some(entry) = entry_opt {
                match &mut entry.desc {
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
        drop(inline);

        // Make all overflow properties non-writable and non-configurable
        let mut overflow = self.overflow_properties.borrow_mut();
        for entry in overflow.iter_mut() {
            match &mut entry.desc {
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

    /// Check if object is frozen
    pub fn is_frozen(&self) -> bool {
        self.flags.borrow().frozen
    }

    /// Seal the object - prevents adding new properties and makes all existing
    /// properties non-configurable
    pub fn seal(&self) {
        let mut flags = self.flags.borrow_mut();
        flags.sealed = true;
        flags.extensible = false;
        drop(flags);

        // Make all inline properties non-configurable
        let mut inline = self.inline_properties.borrow_mut();
        for entry_opt in inline.iter_mut() {
            if let Some(entry) = entry_opt {
                match &mut entry.desc {
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
        drop(inline);

        // Make all overflow properties non-configurable
        let mut overflow = self.overflow_properties.borrow_mut();
        for entry in overflow.iter_mut() {
            match &mut entry.desc {
                PropertyDescriptor::Data { attributes, .. } => {
                    attributes.configurable = false;
                }
                PropertyDescriptor::Accessor { attributes, .. } => {
                    attributes.configurable = false;
                }
                PropertyDescriptor::Deleted => {}
            };
        }
    }

    /// Check if object is sealed
    pub fn is_sealed(&self) -> bool {
        self.flags.borrow().sealed
    }

    /// Prevent extensions - prevents adding new properties
    pub fn prevent_extensions(&self) {
        self.flags.borrow_mut().extensible = false;
    }

    /// Check if object is extensible
    pub fn is_extensible(&self) -> bool {
        self.flags.borrow().extensible
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
            if flags.sparse_array_length.is_some() {
                if (new_len as usize) <= elements.len() {
                    flags.sparse_array_length = None;
                } else {
                    flags.sparse_array_length = Some(new_len);
                }
            }
        } else {
            // Extend with holes
            let new = new_len as usize;
            if new <= MAX_DENSE_PREALLOC {
                self.elements.borrow_mut().resize(new, Value::hole());
                self.flags.borrow_mut().sparse_array_length = None;
            } else {
                self.flags.borrow_mut().sparse_array_length = Some(new_len);
            }
        }
        true
    }

    /// Get array length (for arrays)
    pub fn array_length(&self) -> usize {
        if let Some(sparse_len) = self.flags.borrow().sparse_array_length {
            return sparse_len as usize;
        }
        self.elements.borrow().len()
    }

    /// Push element to array
    pub fn array_push(&self, value: Value) {
        self.elements.borrow_mut().push(value);
    }

    /// Pop element from array
    pub fn array_pop(&self) -> Value {
        self.elements
            .borrow_mut()
            .pop()
            .unwrap_or_else(Value::undefined)
    }

    /// Get inline properties storage (for GC tracing)
    pub(crate) fn get_inline_properties_storage(
        &self,
    ) -> &ObjectCell<[Option<PropertyEntry>; INLINE_PROPERTY_COUNT]> {
        &self.inline_properties
    }

    /// Get overflow properties storage (for GC tracing)
    pub(crate) fn get_overflow_properties_storage(&self) -> &ObjectCell<Vec<PropertyEntry>> {
        &self.overflow_properties
    }

    pub(crate) fn get_elements_storage(&self) -> &ObjectCell<Vec<Value>> {
        &self.elements
    }

    pub(crate) fn get_prototype_storage(&self) -> &ObjectCell<Value> {
        &self.prototype
    }
}

impl std::fmt::Debug for JsObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inline = self.inline_properties.borrow();
        let inline_count = inline.iter().filter(|e| e.is_some()).count();
        let overflow = self.overflow_properties.borrow();
        let flags = self.flags.borrow();
        f.debug_struct("JsObject")
            .field("inline_properties", &inline_count)
            .field("overflow_properties", &overflow.len())
            .field("is_array", &flags.is_array)
            .finish()
    }
}

// SAFETY: JsObject uses ObjectCell (UnsafeCell) for interior mutability.
// Thread confinement is enforced at the VmRuntime/VmContext level.
// Send+Sync is inherited from ObjectCell's explicit impls.
unsafe impl Send for JsObject {}
unsafe impl Sync for JsObject {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_get_set() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = JsObject::new(Value::null(), memory_manager);

        obj.set(PropertyKey::string("foo"), Value::int32(42));
        assert_eq!(obj.get(&PropertyKey::string("foo")), Some(Value::int32(42)));
    }

    #[test]
    fn test_object_has() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = JsObject::new(Value::null(), memory_manager);
        obj.set(PropertyKey::string("foo"), Value::int32(42));

        assert!(obj.has(&PropertyKey::string("foo")));
        assert!(!obj.has(&PropertyKey::string("bar")));
    }

    #[test]
    fn test_array() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let arr = JsObject::array(3, memory_manager);
        assert!(arr.is_array());
        assert_eq!(arr.array_length(), 3);

        arr.set(PropertyKey::Index(0), Value::int32(1));
        arr.set(PropertyKey::Index(1), Value::int32(2));
        arr.set(PropertyKey::Index(2), Value::int32(3));

        assert_eq!(arr.get(&PropertyKey::Index(0)), Some(Value::int32(1)));
        assert_eq!(arr.get(&PropertyKey::Index(1)), Some(Value::int32(2)));
        assert_eq!(arr.get(&PropertyKey::Index(2)), Some(Value::int32(3)));
    }

    #[test]
    fn test_array_holes() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let arr = JsObject::array(3, memory_manager);
        arr.set(PropertyKey::Index(0), Value::int32(10));
        arr.set(PropertyKey::Index(1), Value::int32(20));
        arr.set(PropertyKey::Index(2), Value::int32(30));

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
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let arr = JsObject::array(5, memory_manager);
        // new Array(5) should create holes, not present elements
        assert!(!arr.has_own(&PropertyKey::Index(0)));
        assert!(!arr.has_own(&PropertyKey::Index(4)));
        assert_eq!(arr.array_length(), 5);
    }

    #[test]
    fn test_array_length_truncate() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let arr = JsObject::array(0, memory_manager);
        arr.set(PropertyKey::Index(0), Value::int32(1));
        arr.set(PropertyKey::Index(1), Value::int32(2));
        arr.set(PropertyKey::Index(2), Value::int32(3));
        arr.set(PropertyKey::Index(3), Value::int32(4));
        arr.set(PropertyKey::Index(4), Value::int32(5));
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
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let arr = JsObject::array(0, memory_manager);
        arr.set(PropertyKey::Index(0), Value::int32(10));
        arr.set(PropertyKey::Index(1), Value::int32(20));
        arr.set(PropertyKey::Index(2), Value::int32(30));

        // Setting length via set("length", 1) should truncate
        arr.set(PropertyKey::string("length"), Value::number(1.0));
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
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = JsObject::new(Value::null(), memory_manager);
        obj.set(PropertyKey::string("foo"), Value::int32(42));

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
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = JsObject::new(Value::null(), memory_manager);
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
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj = JsObject::new(Value::null(), memory_manager);
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
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        // Build a prototype chain of depth 100
        let mut proto_val = Value::null();
        for i in 0..100 {
            let obj = GcRef::new(JsObject::new(proto_val, Arc::clone(&memory_manager)));
            obj.set(
                PropertyKey::string(&format!("prop{}", i)),
                Value::int32(i as i32),
            );
            proto_val = Value::object(obj);
        }

        let child = JsObject::new(proto_val, memory_manager);

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
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        // Build a prototype chain that exceeds the limit (100)
        let mut proto_val = Value::null();
        for i in 0..110 {
            let obj = GcRef::new(JsObject::new(proto_val, Arc::clone(&memory_manager)));
            if i == 0 {
                obj.set(PropertyKey::string("deep_prop"), Value::int32(42));
            }
            proto_val = Value::object(obj);
        }

        let child = JsObject::new(proto_val, memory_manager);

        // Property at depth > 100 should not be found (returns None gracefully)
        assert_eq!(child.get(&PropertyKey::string("deep_prop")), None);
        assert!(!child.has(&PropertyKey::string("deep_prop")));
    }

    #[test]
    fn test_prototype_cycle_prevention() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let obj1 = GcRef::new(JsObject::new(Value::null(), Arc::clone(&memory_manager)));
        let obj2 = GcRef::new(JsObject::new(
            Value::object(obj1),
            Arc::clone(&memory_manager),
        ));
        let obj3 = GcRef::new(JsObject::new(
            Value::object(obj2),
            Arc::clone(&memory_manager),
        ));

        // Attempting to create a cycle should fail
        // obj1 -> obj2 -> obj3 -> obj1 would be a cycle
        assert!(!obj1.set_prototype(Value::object(obj3)));

        // Setting to null should work
        assert!(obj1.set_prototype(Value::null()));

        // Setting to an unrelated object should work
        let unrelated = GcRef::new(JsObject::new(Value::null(), memory_manager));
        assert!(obj1.set_prototype(Value::object(unrelated)));
    }
}

// ============================================================================
// GC Tracing Implementation
// ============================================================================

impl otter_vm_gc::GcTraceable for JsObject {
    const NEEDS_TRACE: bool = true;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace prototype (now a Value)
        self.prototype.borrow().trace(tracer);

        // Trace values in inline properties
        for entry_opt in self.inline_properties.borrow().iter() {
            if let Some(entry) = entry_opt {
                entry.desc.trace(tracer);
            }
        }

        // Trace values in overflow properties
        for entry in self.overflow_properties.borrow().iter() {
            entry.desc.trace(tracer);
        }

        // Trace values in dictionary properties
        if let Some(dict) = self.dictionary_properties.borrow().as_ref() {
            for entry in dict.values() {
                entry.desc.trace(tracer);
            }
        }

        // Trace array elements
        for value in self.elements.borrow().iter() {
            value.trace(tracer);
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
