//! JavaScript objects with hidden classes (shapes)
//!
//! Objects use hidden classes (called "shapes") for property access optimization.
//! This is similar to V8's approach.

use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use std::sync::Arc;

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
    /// Create a string property key
    pub fn string(s: &str) -> Self {
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
#[derive(Clone, Copy, Debug, Default)]
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
}

/// A JavaScript object
///
/// Thread-safe with interior mutability.
pub struct JsObject {
    /// Properties storage
    properties: RwLock<FxHashMap<PropertyKey, PropertyDescriptor>>,
    /// Prototype (null for Object.prototype)
    prototype: Option<Arc<JsObject>>,
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
            properties: RwLock::new(FxHashMap::default()),
            prototype,
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

    /// Get property by key
    pub fn get(&self, key: &PropertyKey) -> Option<Value> {
        // Special handling for array "length" property
        if self.is_array() {
            if let PropertyKey::String(s) = key {
                if s.as_str() == "length" {
                    return Some(Value::int32(self.elements.read().len() as i32));
                }
            }
        }

        // Check own properties first
        if let Some(desc) = self.properties.read().get(key) {
            return desc.value().cloned();
        }

        // Check indexed elements for arrays
        if let PropertyKey::Index(i) = key {
            let elements = self.elements.read();
            if (*i as usize) < elements.len() {
                return Some(elements[*i as usize].clone());
            }
        }

        // Check prototype chain
        if let Some(proto) = &self.prototype {
            return proto.get(key);
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
            // Sealed objects cannot have new properties added, but existing can be changed
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
            return false;
        }

        // Check if property exists and is writable
        {
            let props = self.properties.read();
            if let Some(desc) = props.get(&key)
                && !desc.is_writable()
            {
                return false; // Not writable
            }
        }

        // Set or create property
        let property_exists = self.properties.read().contains_key(&key);
        if property_exists || (flags.extensible && !flags.sealed) {
            self.properties
                .write()
                .insert(key, PropertyDescriptor::data(value));
            true
        } else {
            false // Not extensible or sealed
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
        {
            let props = self.properties.read();
            if let Some(desc) = props.get(key) {
                match desc {
                    PropertyDescriptor::Data { attributes, .. }
                    | PropertyDescriptor::Accessor { attributes, .. } => {
                        if !attributes.configurable {
                            return false;
                        }
                    }
                }
            }
        }

        self.properties.write().remove(key).is_some()
    }

    /// Check if object has own property
    pub fn has_own(&self, key: &PropertyKey) -> bool {
        if self.properties.read().contains_key(key) {
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

        if let Some(proto) = &self.prototype {
            return proto.has(key);
        }

        false
    }

    /// Get own property keys
    pub fn own_keys(&self) -> Vec<PropertyKey> {
        let mut keys: Vec<_> = self.properties.read().keys().cloned().collect();

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

        // Can't add new properties if not extensible or sealed
        if !flags.extensible && !self.properties.read().contains_key(&key) {
            return false;
        }

        // Sealed objects can't have new properties
        if flags.sealed && !self.properties.read().contains_key(&key) {
            return false;
        }

        self.properties.write().insert(key, desc);
        true
    }

    /// Get prototype
    pub fn prototype(&self) -> Option<&Arc<JsObject>> {
        self.prototype.as_ref()
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

        // Make all existing properties non-writable and non-configurable
        let mut props = self.properties.write();
        for desc in props.values_mut() {
            match desc {
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

        // Make all existing properties non-configurable
        let mut props = self.properties.write();
        for desc in props.values_mut() {
            match desc {
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
}

impl std::fmt::Debug for JsObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let props = self.properties.read();
        let flags = self.flags.read();
        f.debug_struct("JsObject")
            .field("properties", &props.len())
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

        // CAN delete property (prevent_extensions only stops adding)
        assert!(obj.delete(&PropertyKey::string("foo")));
    }
}
