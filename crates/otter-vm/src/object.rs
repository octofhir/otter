//! Minimal object heap and inline-cache support for the new VM.

use crate::module::FunctionIndex;
use crate::property::PropertyNameId;
use crate::value::RegisterValue;

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

/// Error produced by the minimal object heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectError {
    /// The object handle does not exist in the current heap.
    InvalidHandle,
    /// The heap value exists, but the requested operation is not supported.
    InvalidKind,
    /// The heap value exists, but the requested slot index is out of bounds.
    InvalidIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeapValueKind {
    /// Plain object with named properties.
    Object,
    /// Dense array with indexed elements.
    Array,
    /// String storage with indexed character access.
    String,
    /// Closure object with captured upvalue cells.
    Closure,
    /// Mutable cell used to back one captured upvalue.
    UpvalueCell,
}

#[derive(Debug, Clone, PartialEq)]
enum HeapValue {
    Object {
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<RegisterValue>,
    },
    Array {
        elements: Vec<RegisterValue>,
    },
    String {
        value: Box<str>,
    },
    Closure {
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
    },
    UpvalueCell {
        value: RegisterValue,
    },
}

/// Small object heap used by the early `otter-vm` interpreter.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ObjectHeap {
    objects: Vec<HeapValue>,
    next_shape_id: u64,
}

impl ObjectHeap {
    /// Creates an empty object heap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            objects: Vec::new(),
            next_shape_id: 1,
        }
    }

    /// Allocates a plain empty object.
    pub fn alloc_object(&mut self) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        let shape_id = self.allocate_shape();
        self.objects.push(HeapValue::Object {
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
        });
        handle
    }

    /// Allocates an empty dense array.
    pub fn alloc_array(&mut self) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        self.objects.push(HeapValue::Array {
            elements: Vec::new(),
        });
        handle
    }

    /// Allocates a string value.
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        self.objects.push(HeapValue::String {
            value: value.into(),
        });
        handle
    }

    /// Allocates a mutable upvalue cell.
    pub fn alloc_upvalue(&mut self, value: RegisterValue) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        self.objects.push(HeapValue::UpvalueCell { value });
        handle
    }

    /// Allocates a closure object with captured upvalue cells.
    pub fn alloc_closure(
        &mut self,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
    ) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        self.objects.push(HeapValue::Closure { callee, upvalues });
        handle
    }

    /// Returns the heap-value kind for the given handle.
    pub fn kind(&self, handle: ObjectHandle) -> Result<HeapValueKind, ObjectError> {
        match self.object(handle)? {
            HeapValue::Object { .. } => Ok(HeapValueKind::Object),
            HeapValue::Array { .. } => Ok(HeapValueKind::Array),
            HeapValue::String { .. } => Ok(HeapValueKind::String),
            HeapValue::Closure { .. } => Ok(HeapValueKind::Closure),
            HeapValue::UpvalueCell { .. } => Ok(HeapValueKind::UpvalueCell),
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
            | HeapValue::Closure { .. }
            | HeapValue::UpvalueCell { .. } => Ok(None),
            HeapValue::Array { elements } if property_name == "length" => Ok(Some(
                RegisterValue::from_i32(i32::try_from(elements.len()).unwrap_or(i32::MAX)),
            )),
            HeapValue::String { value } if property_name == "length" => Ok(Some(
                RegisterValue::from_i32(i32::try_from(value.chars().count()).unwrap_or(i32::MAX)),
            )),
            HeapValue::Array { .. } | HeapValue::String { .. } => Ok(None),
        }
    }

    /// Compares two register values with the current strict-equality semantics.
    pub fn strict_eq(&self, lhs: RegisterValue, rhs: RegisterValue) -> Result<bool, ObjectError> {
        if lhs == rhs {
            return Ok(true);
        }

        let Some(lhs_handle) = lhs.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        let Some(rhs_handle) = rhs.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };

        match (self.object(lhs_handle)?, self.object(rhs_handle)?) {
            (HeapValue::String { value: lhs }, HeapValue::String { value: rhs }) => Ok(lhs == rhs),
            _ => Ok(false),
        }
    }

    /// Loads an indexed element from an array or string.
    pub fn get_index(
        &mut self,
        handle: ObjectHandle,
        index: usize,
    ) -> Result<Option<RegisterValue>, ObjectError> {
        match self.object(handle)? {
            HeapValue::Array { elements } => Ok(elements.get(index).copied()),
            HeapValue::String { value } => {
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
            | HeapValue::Closure { .. }
            | HeapValue::UpvalueCell { .. } => Err(ObjectError::InvalidKind),
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
            HeapValue::Array { elements } => {
                if index >= elements.len() {
                    elements.resize(index.saturating_add(1), RegisterValue::undefined());
                }
                elements[index] = value;
                Ok(())
            }
            HeapValue::Object { .. }
            | HeapValue::String { .. }
            | HeapValue::Closure { .. }
            | HeapValue::UpvalueCell { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the callee stored in a closure object.
    pub fn closure_callee(&self, handle: ObjectHandle) -> Result<FunctionIndex, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { callee, .. } => Ok(*callee),
            _ => Err(ObjectError::InvalidKind),
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
    ) -> Result<Option<RegisterValue>, ObjectError> {
        let object = self.object(handle)?;
        let HeapValue::Object {
            shape_id,
            keys,
            values,
        } = object
        else {
            return Err(ObjectError::InvalidKind);
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

    /// Returns a property value through the generic lookup path.
    pub fn get_property(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<(RegisterValue, PropertyInlineCache)>, ObjectError> {
        let object = self.object(handle)?;
        let HeapValue::Object {
            shape_id,
            keys,
            values,
        } = object
        else {
            return Err(ObjectError::InvalidKind);
        };
        let Some(slot_index) = property_slot(keys, property) else {
            return Ok(None);
        };
        let value = values[usize::from(slot_index)];
        let cache = PropertyInlineCache::new(*shape_id, slot_index);
        Ok(Some((value, cache)))
    }

    /// Returns a shaped property value when the shape and slot still match.
    pub fn get_shaped(
        &self,
        handle: ObjectHandle,
        shape_id: ObjectShapeId,
        slot_index: u16,
    ) -> Result<Option<RegisterValue>, ObjectError> {
        let object = self.object(handle)?;
        let HeapValue::Object {
            shape_id: object_shape_id,
            values,
            ..
        } = object
        else {
            return Err(ObjectError::InvalidKind);
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
        let HeapValue::Object {
            shape_id,
            keys,
            values,
        } = object
        else {
            return Err(ObjectError::InvalidKind);
        };
        if *shape_id != cache.shape_id() {
            return Ok(false);
        }

        let slot_index = usize::from(cache.slot_index());
        if keys.get(slot_index) == Some(&property)
            && let Some(slot) = values.get_mut(slot_index)
        {
            *slot = value;
            return Ok(true);
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
        if let Some(slot_index) = match self.object(handle)? {
            HeapValue::Object { keys, .. } => property_slot(keys, property),
            HeapValue::Array { .. }
            | HeapValue::String { .. }
            | HeapValue::Closure { .. }
            | HeapValue::UpvalueCell { .. } => {
                return Err(ObjectError::InvalidKind);
            }
        } {
            let object = self.object_mut(handle)?;
            let HeapValue::Object {
                shape_id, values, ..
            } = object
            else {
                return Err(ObjectError::InvalidKind);
            };
            values[usize::from(slot_index)] = value;
            return Ok(PropertyInlineCache::new(*shape_id, slot_index));
        }

        let shape_id = self.allocate_shape();
        let object = self.object_mut(handle)?;
        let HeapValue::Object {
            shape_id: object_shape_id,
            keys,
            values,
        } = object
        else {
            return Err(ObjectError::InvalidKind);
        };
        keys.push(property);
        values.push(value);
        *object_shape_id = shape_id;
        let slot_index = u16::try_from(values.len().saturating_sub(1)).unwrap_or(u16::MAX);
        Ok(PropertyInlineCache::new(*object_shape_id, slot_index))
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
        let HeapValue::Object {
            shape_id: object_shape_id,
            values,
            ..
        } = object
        else {
            return Err(ObjectError::InvalidKind);
        };
        if *object_shape_id != shape_id {
            return Ok(false);
        }
        let Some(slot) = values.get_mut(usize::from(slot_index)) else {
            return Ok(false);
        };
        *slot = value;
        Ok(true)
    }

    fn object(&self, handle: ObjectHandle) -> Result<&HeapValue, ObjectError> {
        self.objects
            .get(usize::try_from(handle.0).unwrap_or(usize::MAX))
            .ok_or(ObjectError::InvalidHandle)
    }

    fn object_mut(&mut self, handle: ObjectHandle) -> Result<&mut HeapValue, ObjectError> {
        self.objects
            .get_mut(usize::try_from(handle.0).unwrap_or(usize::MAX))
            .ok_or(ObjectError::InvalidHandle)
    }

    fn allocate_shape(&mut self) -> ObjectShapeId {
        let shape_id = ObjectShapeId(self.next_shape_id);
        self.next_shape_id = self.next_shape_id.saturating_add(1);
        shape_id
    }
}

fn property_slot(keys: &[PropertyNameId], property: PropertyNameId) -> Option<u16> {
    keys.iter()
        .position(|key| *key == property)
        .and_then(|index| u16::try_from(index).ok())
}

#[cfg(test)]
mod tests {
    use crate::module::FunctionIndex;
    use crate::property::PropertyNameId;
    use crate::value::RegisterValue;

    use super::{HeapValueKind, ObjectError, ObjectHeap, PropertyInlineCache};

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
            Some(RegisterValue::from_i32(7))
        );

        assert!(
            heap.set_cached(handle, property, RegisterValue::from_i32(9), cache)
                .expect("cache store should succeed")
        );

        let generic = heap
            .get_property(handle, property)
            .expect("generic lookup should succeed");
        assert_eq!(
            generic,
            Some((
                RegisterValue::from_i32(9),
                PropertyInlineCache::new(cache.shape_id(), cache.slot_index())
            ))
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
        let closure = heap.alloc_closure(FunctionIndex(7), vec![upvalue]);

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
}
