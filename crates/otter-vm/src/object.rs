//! Minimal object heap and inline-cache support for the new VM.

use crate::host::HostFunctionId;
use crate::module::FunctionIndex;
use crate::payload::NativePayloadId;
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
    /// Mutable cell used to back one captured upvalue.
    UpvalueCell,
    /// Internal iterator used by the new VM iteration lowering.
    Iterator,
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

/// Property slot stored on ordinary or host-function objects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropertyValue {
    Data(RegisterValue),
    Accessor {
        getter: Option<ObjectHandle>,
        setter: Option<ObjectHandle>,
    },
}

impl PropertyValue {
    #[must_use]
    pub const fn data(value: RegisterValue) -> Self {
        Self::Data(value)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum HeapValue {
    Object {
        prototype: Option<ObjectHandle>,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
    },
    NativeObject {
        prototype: Option<ObjectHandle>,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        payload: NativePayloadId,
    },
    Array {
        prototype: Option<ObjectHandle>,
        elements: Vec<RegisterValue>,
    },
    String {
        prototype: Option<ObjectHandle>,
        value: Box<str>,
    },
    Closure {
        prototype: Option<ObjectHandle>,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
    },
    HostFunction {
        function: HostFunctionId,
        prototype: Option<ObjectHandle>,
        shape_id: ObjectShapeId,
        keys: Vec<PropertyNameId>,
        values: Vec<PropertyValue>,
    },
    UpvalueCell {
        value: RegisterValue,
    },
    ArrayIterator {
        iterable: ObjectHandle,
        next_index: usize,
        closed: bool,
    },
    StringIterator {
        iterable: ObjectHandle,
        next_index: usize,
        closed: bool,
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
            prototype: None,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
        });
        handle
    }

    /// Allocates an ordinary object that carries one native payload link.
    pub fn alloc_native_object(&mut self, payload: NativePayloadId) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        let shape_id = self.allocate_shape();
        self.objects.push(HeapValue::NativeObject {
            prototype: None,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            payload,
        });
        handle
    }

    /// Allocates an empty dense array.
    pub fn alloc_array(&mut self) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        self.objects.push(HeapValue::Array {
            prototype: None,
            elements: Vec::new(),
        });
        handle
    }

    /// Allocates a string value.
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        self.objects.push(HeapValue::String {
            prototype: None,
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
        let shape_id = self.allocate_shape();
        self.objects.push(HeapValue::Closure {
            prototype: None,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
            callee,
            upvalues,
        });
        handle
    }

    /// Allocates a host-callable native function object.
    pub fn alloc_host_function(&mut self, function: HostFunctionId) -> ObjectHandle {
        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        let shape_id = self.allocate_shape();
        self.objects.push(HeapValue::HostFunction {
            function,
            prototype: None,
            shape_id,
            keys: Vec::new(),
            values: Vec::new(),
        });
        handle
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
            HeapValue::UpvalueCell { .. } => Ok(HeapValueKind::UpvalueCell),
            HeapValue::ArrayIterator { .. } | HeapValue::StringIterator { .. } => {
                Ok(HeapValueKind::Iterator)
            }
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
            | HeapValue::HostFunction { prototype, .. } => Ok(*prototype),
            HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Updates the direct prototype link for the given heap value.
    pub fn set_prototype(
        &mut self,
        handle: ObjectHandle,
        prototype: Option<ObjectHandle>,
    ) -> Result<(), ObjectError> {
        if let Some(prototype) = prototype {
            self.object(prototype)?;
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
            } => {
                *slot = prototype;
                Ok(())
            }
            HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. } => Err(ObjectError::InvalidKind),
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
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. } => Ok(None),
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
            (HeapValue::String { value: lhs, .. }, HeapValue::String { value: rhs, .. }) => {
                Ok(lhs == rhs)
            }
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
            HeapValue::Array { elements, .. } => Ok(elements.get(index).copied()),
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
            | HeapValue::StringIterator { .. } => Err(ObjectError::InvalidKind),
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
            HeapValue::Array { elements, .. } => {
                if index >= elements.len() {
                    elements.resize(index.saturating_add(1), RegisterValue::undefined());
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
            | HeapValue::StringIterator { .. } => Err(ObjectError::InvalidKind),
        }
    }

    /// Allocates an internal iterator for a supported iterable.
    pub fn alloc_iterator(&mut self, iterable: ObjectHandle) -> Result<ObjectHandle, ObjectError> {
        let iterator = match self.object(iterable)? {
            HeapValue::Array { .. } => HeapValue::ArrayIterator {
                iterable,
                next_index: 0,
                closed: false,
            },
            HeapValue::String { .. } => HeapValue::StringIterator {
                iterable,
                next_index: 0,
                closed: false,
            },
            _ => return Err(ObjectError::InvalidKind),
        };

        let handle = ObjectHandle(u32::try_from(self.objects.len()).unwrap_or(u32::MAX));
        self.objects.push(iterator);
        Ok(handle)
    }

    /// Advances an internal iterator by one step.
    pub fn iterator_next(&mut self, handle: ObjectHandle) -> Result<IteratorStep, ObjectError> {
        enum IteratorKind {
            Array,
            String,
        }

        let (iterable, next_index, closed, kind) = match self.object(handle)? {
            HeapValue::ArrayIterator {
                iterable,
                next_index,
                closed,
            } => (*iterable, *next_index, *closed, IteratorKind::Array),
            HeapValue::StringIterator {
                iterable,
                next_index,
                closed,
            } => (*iterable, *next_index, *closed, IteratorKind::String),
            _ => return Err(ObjectError::InvalidKind),
        };

        if closed {
            return Ok(IteratorStep::done());
        }

        let step = match kind {
            IteratorKind::Array | IteratorKind::String => {
                match self.get_index(iterable, next_index)? {
                    Some(value) => IteratorStep::yield_value(value),
                    None => IteratorStep::done(),
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
                    *next_index = next_index.saturating_add(1);
                }
            }
            _ => return Err(ObjectError::InvalidKind),
        }

        Ok(step)
    }

    /// Closes an internal iterator.
    pub fn iterator_close(&mut self, handle: ObjectHandle) -> Result<(), ObjectError> {
        match self.object_mut(handle)? {
            HeapValue::ArrayIterator { closed, .. } | HeapValue::StringIterator { closed, .. } => {
                *closed = true;
                Ok(())
            }
            _ => Err(ObjectError::InvalidKind),
        }
    }

    /// Returns the callee stored in a closure object.
    pub fn closure_callee(&self, handle: ObjectHandle) -> Result<FunctionIndex, ObjectError> {
        match self.object(handle)? {
            HeapValue::Closure { callee, .. } => Ok(*callee),
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
            | HeapValue::StringIterator { .. } => Ok(None),
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
            | HeapValue::StringIterator { .. } => Ok(None),
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
            | HeapValue::StringIterator { .. } => Ok(None),
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
            | HeapValue::StringIterator { .. } => Ok(None),
        }
    }

    /// Visits every live native payload link stored in the heap.
    pub fn trace_native_payload_links(
        &self,
        tracer: &mut dyn FnMut(ObjectHandle, NativePayloadId),
    ) {
        for (index, object) in self.objects.iter().enumerate() {
            let HeapValue::NativeObject { payload, .. } = object else {
                continue;
            };
            let handle = ObjectHandle(u32::try_from(index).unwrap_or(u32::MAX));
            tracer(handle, *payload);
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
        let mut current = Some(handle);
        while let Some(owner) = current {
            if let Some((value, cache)) = self.get_own_property(owner, property)? {
                let cache = (owner == handle).then_some(cache);
                return Ok(Some(PropertyLookup::new(owner, value, cache)));
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
            *slot = PropertyValue::data(value);
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
            HeapValue::NativeObject { keys, .. } => property_slot(keys, property),
            HeapValue::Closure { keys, .. } => property_slot(keys, property),
            HeapValue::HostFunction { keys, .. } => property_slot(keys, property),
            HeapValue::Array { .. }
            | HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. } => {
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
                } => (shape_id, values),
                _ => return Err(ObjectError::InvalidKind),
            };
            values[usize::from(slot_index)] = PropertyValue::data(value);
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
            } => (object_shape_id, keys, values),
            _ => return Err(ObjectError::InvalidKind),
        };
        keys.push(property);
        values.push(PropertyValue::data(value));
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
        let accessor = PropertyValue::Accessor { getter, setter };

        if let Some(slot_index) = match self.object(handle)? {
            HeapValue::Object { keys, .. } => property_slot(keys, property),
            HeapValue::NativeObject { keys, .. } => property_slot(keys, property),
            HeapValue::Closure { keys, .. } => property_slot(keys, property),
            HeapValue::HostFunction { keys, .. } => property_slot(keys, property),
            HeapValue::Array { .. }
            | HeapValue::String { .. }
            | HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. } => return Err(ObjectError::InvalidKind),
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

    fn get_own_property(
        &self,
        handle: ObjectHandle,
        property: PropertyNameId,
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
            } => (shape_id, keys, values),
            HeapValue::Array { .. } | HeapValue::String { .. } => {
                return Ok(None);
            }
            HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. } => return Err(ObjectError::InvalidKind),
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
            | HeapValue::HostFunction { prototype, .. } => Ok(*prototype),
            HeapValue::UpvalueCell { .. }
            | HeapValue::ArrayIterator { .. }
            | HeapValue::StringIterator { .. } => Err(ObjectError::InvalidKind),
        }
    }
}

fn property_slot(keys: &[PropertyNameId], property: PropertyNameId) -> Option<u16> {
    keys.iter()
        .position(|key| *key == property)
        .and_then(|index| u16::try_from(index).ok())
}

#[cfg(test)]
mod tests {
    use crate::host::HostFunctionId;
    use crate::module::FunctionIndex;
    use crate::property::PropertyNameId;
    use crate::value::RegisterValue;

    use crate::payload::NativePayloadId;

    use super::{
        HeapValueKind, IteratorStep, ObjectError, ObjectHeap, PropertyInlineCache, PropertyValue,
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
            Some(PropertyValue::Data(RegisterValue::from_i32(7)))
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
                let PropertyValue::Data(value) = lookup.value() else {
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
            PropertyValue::Data(RegisterValue::from_i32(7))
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
            Some(PropertyValue::Data(RegisterValue::from_i32(9)))
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
            Some(PropertyValue::Data(RegisterValue::from_i32(11)))
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
