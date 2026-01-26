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

use parking_lot::RwLock;
use std::sync::Arc;

use crate::shape::Shape;

/// Maximum prototype chain depth to prevent stack overflow
const MAX_PROTOTYPE_CHAIN_DEPTH: usize = 100;

/// Number of properties stored inline in the object (JSC-style optimization)
/// Properties beyond this count overflow to a Vec.
pub const INLINE_PROPERTY_COUNT: usize = 4;
use crate::string::JsString;
use crate::value::Value;

/// Property key (string or symbol)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PropertyKey {
    /// String property key
    String(Arc<JsString>),
    /// Symbol property key
    Symbol(u64),
    /// Integer index (for arrays)
    Index(u32),
}

impl PropertyKey {
    /// Create a string property key (canonicalizes numeric strings to Index)
    pub fn string(s: &str) -> Self {
        // Canonicalize numeric strings to Index for consistent lookup
        if let Ok(n) = s.parse::<u32>() {
            // Verify it's canonical (no leading zeros except for "0")
            if n.to_string() == s {
                return Self::Index(n);
            }
        }
        Self::String(JsString::intern(s))
    }

    /// Create from a string Arc
    pub fn from_js_string(s: Arc<JsString>) -> Self {
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
                tracer.mark_header(s.gc_header() as *const _);
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
    /// Default data property attributes
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
}

impl PropertyDescriptor {
    /// Create a data property
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

    /// Get the value (for data properties)
    pub fn value(&self) -> Option<&Value> {
        match self {
            Self::Data { value, .. } => Some(value),
            Self::Accessor { .. } => None,
        }
    }

    /// Get value mutably
    pub fn value_mut(&mut self) -> Option<&mut Value> {
        match self {
            Self::Data { value, .. } => Some(value),
            Self::Accessor { .. } => None,
        }
    }

    /// Check if writable
    pub fn is_writable(&self) -> bool {
        match self {
            Self::Data { attributes, .. } => attributes.writable,
            Self::Accessor { .. } => false,
        }
    }

    /// Check if configurable
    pub fn is_configurable(&self) -> bool {
        match self {
            Self::Data { attributes, .. } | Self::Accessor { attributes, .. } => {
                attributes.configurable
            }
        }
    }

    /// Check if enumerable
    pub fn enumerable(&self) -> bool {
        match self {
            Self::Data { attributes, .. } | Self::Accessor { attributes, .. } => {
                attributes.enumerable
            }
        }
    }
}

/// Internal property storage entry
#[derive(Clone, Debug)]
pub(crate) struct PropertyEntry {
    /// Descriptor for the property (Data or Accessor)
    pub(crate) desc: PropertyDescriptor,
}

/// A JavaScript object
///
/// Thread-safe with interior mutability.
///
/// ## Inline Properties
///
/// The first `INLINE_PROPERTY_COUNT` properties are stored inline in the object
/// for faster access. Additional properties overflow to the `properties` Vec.
/// Both inline and overflow use `PropertyEntry` to support accessor properties.
pub struct JsObject {
    /// Current shape of the object
    shape: RwLock<Arc<Shape>>,
    /// Inline property storage for first N properties (JSC-style)
    inline_properties: RwLock<[Option<PropertyEntry>; INLINE_PROPERTY_COUNT]>,
    /// Overflow properties storage (for properties beyond INLINE_PROPERTY_COUNT)
    overflow_properties: RwLock<Vec<PropertyEntry>>,
    /// Prototype (null for Object.prototype, mutable via Reflect.setPrototypeOf)
    prototype: RwLock<Option<Arc<JsObject>>>,
    /// Array elements (for array-like objects)
    elements: RwLock<Vec<Value>>,
    /// Object flags (mutable for freeze/seal/preventExtensions)
    flags: RwLock<ObjectFlags>,
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
}

impl JsObject {
    /// Create a new empty object
    pub fn new(prototype: Option<Arc<JsObject>>) -> Self {
        Self {
            shape: RwLock::new(Shape::root()),
            inline_properties: RwLock::new([None, None, None, None]),
            overflow_properties: RwLock::new(Vec::new()),
            prototype: RwLock::new(prototype),
            elements: RwLock::new(Vec::new()),
            flags: RwLock::new(ObjectFlags {
                extensible: true,
                ..Default::default()
            }),
        }
    }

    /// Create a new array
    pub fn array(length: usize) -> Self {
        let obj = Self::new(None); // TODO: Array.prototype
        obj.flags.write().is_array = true;
        obj.elements.write().resize(length, Value::undefined());
        obj
    }

    /// Get property value by offset (for Inline Cache fast path)
    /// First INLINE_PROPERTY_COUNT properties are stored inline, rest in overflow.
    /// Returns None for accessor properties - caller should use get_property_entry_by_offset instead.
    #[inline]
    pub fn get_by_offset(&self, offset: usize) -> Option<Value> {
        if offset < INLINE_PROPERTY_COUNT {
            let inline = self.inline_properties.read();
            inline[offset]
                .as_ref()
                .and_then(|e| e.desc.value().cloned())
        } else {
            let overflow = self.overflow_properties.read();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            overflow
                .get(overflow_idx)
                .and_then(|e| e.desc.value().cloned())
        }
    }

    /// Get property entry by offset (includes accessor properties)
    #[inline]
    pub fn get_property_entry_by_offset(&self, offset: usize) -> Option<PropertyDescriptor> {
        if offset < INLINE_PROPERTY_COUNT {
            let inline = self.inline_properties.read();
            inline[offset].as_ref().map(|e| e.desc.clone())
        } else {
            let overflow = self.overflow_properties.read();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            overflow.get(overflow_idx).map(|e| e.desc.clone())
        }
    }

    /// Set property by offset (for Inline Cache fast path)
    /// First INLINE_PROPERTY_COUNT properties are stored inline, rest in overflow.
    #[inline]
    pub fn set_by_offset(&self, offset: usize, value: Value) -> bool {
        if offset < INLINE_PROPERTY_COUNT {
            let mut inline = self.inline_properties.write();
            if let Some(entry) = inline[offset].as_mut() {
                if entry.desc.is_writable() {
                    if let PropertyDescriptor::Data {
                        value: ref mut v, ..
                    } = entry.desc
                    {
                        *v = value;
                        return true;
                    }
                }
            }
            false
        } else {
            let mut overflow = self.overflow_properties.write();
            let overflow_idx = offset - INLINE_PROPERTY_COUNT;
            if let Some(entry) = overflow.get_mut(overflow_idx) {
                if entry.desc.is_writable() {
                    if let PropertyDescriptor::Data {
                        value: ref mut v, ..
                    } = entry.desc
                    {
                        *v = value;
                        return true;
                    }
                }
            }
            false
        }
    }

    /// Get total property count (inline + overflow)
    #[allow(dead_code)]
    fn property_count(&self) -> usize {
        let inline = self.inline_properties.read();
        let inline_count = inline.iter().filter(|v| v.is_some()).count();
        let overflow = self.overflow_properties.read();
        inline_count + overflow.len()
    }

    /// Get current shape
    pub fn shape(&self) -> Arc<Shape> {
        self.shape.read().clone()
    }

    /// Get property by key
    pub fn get(&self, key: &PropertyKey) -> Option<Value> {
        // Special handling for array "length" property
        if self.is_array()
            && let PropertyKey::String(s) = key
            && s.as_str() == "length"
        {
            return Some(Value::int32(self.elements.read().len() as i32));
        }

        // Check own properties first via shape lookup
        {
            let shape = self.shape.read();
            if let Some(offset) = shape.get_offset(key) {
                return self.get_by_offset(offset);
            }
        }

        // Check indexed elements for arrays
        if let PropertyKey::Index(i) = key {
            let elements = self.elements.read();
            if (*i as usize) < elements.len() {
                return Some(elements[*i as usize].clone());
            }
            drop(elements);
            // For non-arrays, also try string property lookup
            let string_key = PropertyKey::String(crate::string::JsString::intern(&i.to_string()));
            return self.get(&string_key);
        }

        // Check prototype chain iteratively to avoid stack overflow
        let mut current: Option<Arc<JsObject>> = self.prototype.read().clone();
        let mut depth = 0;

        while let Some(proto) = current {
            depth += 1;
            // Optimization/Safety: limit prototype chain depth
            if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                break;
            }

            // Check proto via shape lookup and inline storage
            {
                let shape = proto.shape.read();
                if let Some(offset) = shape.get_offset(key) {
                    return proto.get_by_offset(offset);
                }
            }

            current = proto.prototype.read().clone();
        }

        None
    }

    /// Extract all values held by this object and clear storage.
    /// Used for iterative destruction to prevent stack overflow.
    pub fn clear_and_extract_values(&self) -> Vec<Value> {
        let mut values = Vec::new();

        // Clear inline properties
        {
            let mut inline = self.inline_properties.write();
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
                    }
                }
            }
        }

        // Clear overflow properties
        {
            let mut overflow = self.overflow_properties.write();
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
                }
            }
        }

        // Clear elements
        {
            let mut elems = self.elements.write();
            for val in elems.drain(..) {
                values.push(val);
            }
        }

        // Clear prototype
        {
            let mut proto = self.prototype.write();
            if let Some(p) = proto.take() {
                values.push(Value::object(p));
            }
        }

        values
    }

    /// Get own property descriptor (does not walk prototype chain).
    pub fn get_own_property_descriptor(&self, key: &PropertyKey) -> Option<PropertyDescriptor> {
        let shape = self.shape.read();
        if let Some(offset) = shape.get_offset(key) {
            return self.get_property_entry_by_offset(offset);
        }
        None
    }

    /// Lookup property descriptor (walks prototype chain).
    pub fn lookup_property_descriptor(&self, key: &PropertyKey) -> Option<PropertyDescriptor> {
        if let Some(desc) = self.get_own_property_descriptor(key) {
            return Some(desc);
        }

        // Walk prototype chain iteratively to avoid stack overflow
        let mut current: Option<Arc<JsObject>> = self.prototype.read().clone();
        let mut depth = 0;

        while let Some(proto) = current {
            depth += 1;
            if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                return None; // Limit reached
            }

            if let Some(desc) = proto.get_own_property_descriptor(key) {
                return Some(desc);
            }

            current = proto.prototype.read().clone();
        }

        None
    }

    /// Set property by key
    pub fn set(&self, key: PropertyKey, value: Value) -> bool {
        let flags = self.flags.read();

        // Frozen objects cannot have properties changed
        if flags.frozen {
            return false;
        }

        // Handle indexed elements for arrays
        if let PropertyKey::Index(i) = &key {
            let mut elements = self.elements.write();
            let idx = *i as usize;
            if idx < elements.len() {
                elements[idx] = value;
                return true;
            } else if flags.is_array && flags.extensible && !flags.sealed {
                // Extend array (only if extensible and not sealed)
                elements.resize(idx + 1, Value::undefined());
                elements[idx] = value;
                return true;
            }
            drop(elements);
            drop(flags);
            // For non-arrays, fall through to store as string property
            let string_key = PropertyKey::String(crate::string::JsString::intern(&i.to_string()));
            return self.set(string_key, value);
        }

        // Check if property exists
        {
            let shape = self.shape.read();
            if let Some(offset) = shape.get_offset(&key) {
                // Property exists, use set_by_offset
                drop(shape);
                drop(flags);
                return self.set_by_offset(offset, value);
            }
        }

        // New property addition
        if flags.extensible && !flags.sealed {
            let mut shape_write = self.shape.write();

            // Get current property count to determine where to store
            let inline = self.inline_properties.read();
            let current_inline_count = inline.iter().filter(|v| v.is_some()).count();
            drop(inline);

            // Transition to new shape
            let next_shape = shape_write.transition(key);
            *shape_write = next_shape;

            let entry = PropertyEntry {
                desc: PropertyDescriptor::data(value),
            };

            if current_inline_count < INLINE_PROPERTY_COUNT {
                // Store in inline slot
                let mut inline = self.inline_properties.write();
                inline[current_inline_count] = Some(entry);
            } else {
                // Store in overflow
                let mut overflow = self.overflow_properties.write();
                overflow.push(entry);
            }
            true
        } else {
            false
        }
    }

    /// Delete property
    pub fn delete(&self, key: &PropertyKey) -> bool {
        // Sealed or frozen objects cannot have properties deleted
        let flags = self.flags.read();
        if flags.sealed || flags.frozen {
            return false;
        }
        drop(flags);

        // Check if configurable
        if let Some(desc) = self.get_own_property_descriptor(key) {
            if !desc.is_configurable() {
                return false;
            }

            // Note: Deleting properties breaks the Shape transition model.
            // Modern engines usually transition to a "Dictionary Mode" shape.
            // For simplicity, we just return false and don't actually delete from Vec
            return false;
        }

        false
    }

    /// Check if object has own property
    pub fn has_own(&self, key: &PropertyKey) -> bool {
        if self.shape.read().get_offset(key).is_some() {
            return true;
        }

        // Check indexed elements
        if let PropertyKey::Index(i) = key {
            let elements = self.elements.read();
            return (*i as usize) < elements.len();
        }

        false
    }

    /// Check if object has property (including prototype chain)
    pub fn has(&self, key: &PropertyKey) -> bool {
        if self.has_own(key) {
            return true;
        }

        // Walk prototype chain iteratively to avoid stack overflow
        let mut current: Option<Arc<JsObject>> = self.prototype.read().clone();
        let mut depth = 0;

        while let Some(proto) = current {
            depth += 1;
            if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                return false; // Limit reached
            }

            if proto.has_own(key) {
                return true;
            }

            current = proto.prototype.read().clone();
        }

        false
    }

    /// Get own property keys
    pub fn own_keys(&self) -> Vec<PropertyKey> {
        let mut keys = self.shape.read().own_keys();

        // Add indexed elements
        let elements = self.elements.read();
        for i in 0..elements.len() {
            keys.push(PropertyKey::Index(i as u32));
        }

        keys
    }

    /// Define a property with descriptor
    pub fn define_property(&self, key: PropertyKey, desc: PropertyDescriptor) -> bool {
        let flags = self.flags.read();

        // Frozen objects cannot have properties defined
        if flags.frozen {
            return false;
        }

        // Check if exists
        let offset = self.shape.read().get_offset(&key);

        if let Some(off) = offset {
            // Update existing property
            if off < INLINE_PROPERTY_COUNT {
                let mut inline = self.inline_properties.write();
                if let Some(entry) = inline[off].as_mut() {
                    entry.desc = desc;
                    return true;
                }
            } else {
                let mut overflow = self.overflow_properties.write();
                let overflow_idx = off - INLINE_PROPERTY_COUNT;
                if let Some(entry) = overflow.get_mut(overflow_idx) {
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

        let mut shape_write = self.shape.write();

        // Get current inline count
        let inline = self.inline_properties.read();
        let current_inline_count = inline.iter().filter(|v| v.is_some()).count();
        drop(inline);

        // Transition to new shape
        let next_shape = shape_write.transition(key);
        *shape_write = next_shape;

        let entry = PropertyEntry { desc };

        if current_inline_count < INLINE_PROPERTY_COUNT {
            // Store in inline slot
            let mut inline = self.inline_properties.write();
            inline[current_inline_count] = Some(entry);
        } else {
            // Store in overflow
            let mut overflow = self.overflow_properties.write();
            overflow.push(entry);
        }
        true
    }

    /// Get prototype
    pub fn prototype(&self) -> Option<Arc<JsObject>> {
        self.prototype.read().clone()
    }

    /// Set prototype
    /// Returns false if object is not extensible, if it would create a cycle,
    /// or if the chain would be too deep
    pub fn set_prototype(&self, prototype: Option<Arc<JsObject>>) -> bool {
        if !self.flags.read().extensible {
            return false;
        }

        // Check for cycles and excessive depth
        if let Some(ref proto) = prototype {
            let self_ptr = self as *const JsObject;
            let mut current = Some(Arc::clone(proto));
            let mut depth = 0;

            while let Some(p) = current {
                depth += 1;
                if depth > MAX_PROTOTYPE_CHAIN_DEPTH {
                    return false; // Chain would be too deep
                }
                if Arc::as_ptr(&p) as *const JsObject == self_ptr {
                    return false; // Would create cycle
                }
                current = p.prototype.read().clone();
            }
        }

        *self.prototype.write() = prototype;
        true
    }

    /// Check if object is an array
    pub fn is_array(&self) -> bool {
        self.flags.read().is_array
    }

    // ========================================================================
    // Object.freeze / Object.seal / Object.preventExtensions
    // ========================================================================

    /// Freeze the object - makes all properties non-writable and non-configurable,
    /// and prevents adding new properties
    pub fn freeze(&self) {
        let mut flags = self.flags.write();
        flags.frozen = true;
        flags.sealed = true;
        flags.extensible = false;
        drop(flags);

        // Note: Inline properties store only values with implicit default (writable) attrs.
        // When frozen, we could move them to overflow to track frozen state, but for
        // simplicity we just set the frozen flag and check it on write operations.

        // Make all overflow properties non-writable and non-configurable
        let mut overflow = self.overflow_properties.write();
        for entry in overflow.iter_mut() {
            match &mut entry.desc {
                PropertyDescriptor::Data { attributes, .. } => {
                    attributes.writable = false;
                    attributes.configurable = false;
                }
                PropertyDescriptor::Accessor { attributes, .. } => {
                    attributes.configurable = false;
                }
            }
        }
    }

    /// Check if object is frozen
    pub fn is_frozen(&self) -> bool {
        self.flags.read().frozen
    }

    /// Seal the object - prevents adding new properties and makes all existing
    /// properties non-configurable
    pub fn seal(&self) {
        let mut flags = self.flags.write();
        flags.sealed = true;
        flags.extensible = false;
        drop(flags);

        // Make all overflow properties non-configurable
        // (inline properties are implicitly configurable by default)
        let mut overflow = self.overflow_properties.write();
        for entry in overflow.iter_mut() {
            match &mut entry.desc {
                PropertyDescriptor::Data { attributes, .. }
                | PropertyDescriptor::Accessor { attributes, .. } => {
                    attributes.configurable = false;
                }
            }
        }
    }

    /// Check if object is sealed
    pub fn is_sealed(&self) -> bool {
        self.flags.read().sealed
    }

    /// Prevent extensions - prevents adding new properties
    pub fn prevent_extensions(&self) {
        self.flags.write().extensible = false;
    }

    /// Check if object is extensible
    pub fn is_extensible(&self) -> bool {
        self.flags.read().extensible
    }

    /// Get array length (for arrays)
    pub fn array_length(&self) -> usize {
        self.elements.read().len()
    }

    /// Push element to array
    pub fn array_push(&self, value: Value) {
        self.elements.write().push(value);
    }

    /// Pop element from array
    pub fn array_pop(&self) -> Value {
        self.elements.write().pop().unwrap_or_else(Value::undefined)
    }

    /// Get inline properties storage (for GC tracing)
    pub(crate) fn get_inline_properties_storage(
        &self,
    ) -> &RwLock<[Option<PropertyEntry>; INLINE_PROPERTY_COUNT]> {
        &self.inline_properties
    }

    /// Get overflow properties storage (for GC tracing)
    pub(crate) fn get_overflow_properties_storage(&self) -> &RwLock<Vec<PropertyEntry>> {
        &self.overflow_properties
    }

    pub(crate) fn get_elements_storage(&self) -> &RwLock<Vec<Value>> {
        &self.elements
    }
}

impl std::fmt::Debug for JsObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inline = self.inline_properties.read();
        let inline_count = inline.iter().filter(|e| e.is_some()).count();
        let overflow = self.overflow_properties.read();
        let flags = self.flags.read();
        f.debug_struct("JsObject")
            .field("inline_properties", &inline_count)
            .field("overflow_properties", &overflow.len())
            .field("is_array", &flags.is_array)
            .finish()
    }
}

// SAFETY: JsObject uses RwLock for interior mutability
unsafe impl Send for JsObject {}
unsafe impl Sync for JsObject {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_get_set() {
        let obj = JsObject::new(None);

        obj.set(PropertyKey::string("foo"), Value::int32(42));
        assert_eq!(obj.get(&PropertyKey::string("foo")), Some(Value::int32(42)));
    }

    #[test]
    fn test_object_has() {
        let obj = JsObject::new(None);
        obj.set(PropertyKey::string("foo"), Value::int32(42));

        assert!(obj.has(&PropertyKey::string("foo")));
        assert!(!obj.has(&PropertyKey::string("bar")));
    }

    #[test]
    fn test_array() {
        let arr = JsObject::array(3);
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
    fn test_object_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JsObject>();
    }

    #[test]
    fn test_object_freeze() {
        let obj = JsObject::new(None);
        obj.set(PropertyKey::string("foo"), Value::int32(42));

        assert!(!obj.is_frozen());
        assert!(obj.is_extensible());

        obj.freeze();

        assert!(obj.is_frozen());
        assert!(obj.is_sealed());
        assert!(!obj.is_extensible());

        // Cannot modify existing property
        assert!(!obj.set(PropertyKey::string("foo"), Value::int32(100)));
        assert_eq!(obj.get(&PropertyKey::string("foo")), Some(Value::int32(42)));

        // Cannot add new property
        assert!(!obj.set(PropertyKey::string("bar"), Value::int32(200)));
        assert_eq!(obj.get(&PropertyKey::string("bar")), None);

        // Cannot delete property
        assert!(!obj.delete(&PropertyKey::string("foo")));
        assert!(obj.has_own(&PropertyKey::string("foo")));
    }

    #[test]
    fn test_object_seal() {
        let obj = JsObject::new(None);
        obj.set(PropertyKey::string("foo"), Value::int32(42));

        assert!(!obj.is_sealed());

        obj.seal();

        assert!(obj.is_sealed());
        assert!(!obj.is_frozen());
        assert!(!obj.is_extensible());

        // CAN modify existing property (seal allows writes, freeze doesn't)
        assert!(obj.set(PropertyKey::string("foo"), Value::int32(100)));
        assert_eq!(
            obj.get(&PropertyKey::string("foo")),
            Some(Value::int32(100))
        );

        // Cannot add new property
        assert!(!obj.set(PropertyKey::string("bar"), Value::int32(200)));
        assert_eq!(obj.get(&PropertyKey::string("bar")), None);

        // Cannot delete property
        assert!(!obj.delete(&PropertyKey::string("foo")));
    }

    #[test]
    fn test_object_prevent_extensions() {
        let obj = JsObject::new(None);
        obj.set(PropertyKey::string("foo"), Value::int32(42));

        assert!(obj.is_extensible());

        obj.prevent_extensions();

        assert!(!obj.is_extensible());
        assert!(!obj.is_sealed());
        assert!(!obj.is_frozen());

        // CAN modify existing property
        assert!(obj.set(PropertyKey::string("foo"), Value::int32(100)));

        // Cannot add new property
        assert!(!obj.set(PropertyKey::string("bar"), Value::int32(200)));

        // Deleting property is not supported yet
        assert!(!obj.delete(&PropertyKey::string("foo")));
    }

    #[test]
    fn test_deep_prototype_chain() {
        // Build a prototype chain of depth 100
        let mut proto: Option<Arc<JsObject>> = None;
        for i in 0..100 {
            let obj = Arc::new(JsObject::new(proto.clone()));
            obj.set(
                PropertyKey::string(&format!("prop{}", i)),
                Value::int32(i as i32),
            );
            proto = Some(obj);
        }

        let child = JsObject::new(proto);

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
        // Build a prototype chain that exceeds the limit (100)
        let mut proto: Option<Arc<JsObject>> = None;
        for i in 0..110 {
            let obj = Arc::new(JsObject::new(proto.clone()));
            if i == 0 {
                obj.set(PropertyKey::string("deep_prop"), Value::int32(42));
            }
            proto = Some(obj);
        }

        let child = JsObject::new(proto);

        // Property at depth > 100 should not be found (returns None gracefully)
        assert_eq!(child.get(&PropertyKey::string("deep_prop")), None);
        assert!(!child.has(&PropertyKey::string("deep_prop")));
    }

    #[test]
    fn test_prototype_cycle_prevention() {
        let obj1 = Arc::new(JsObject::new(None));
        let obj2 = Arc::new(JsObject::new(Some(obj1.clone())));
        let obj3 = Arc::new(JsObject::new(Some(obj2.clone())));

        // Attempting to create a cycle should fail
        // obj1 -> obj2 -> obj3 -> obj1 would be a cycle
        assert!(!obj1.set_prototype(Some(obj3.clone())));

        // Setting to None should work
        assert!(obj1.set_prototype(None));

        // Setting to an unrelated object should work
        let unrelated = Arc::new(JsObject::new(None));
        assert!(obj1.set_prototype(Some(unrelated)));
    }
}
