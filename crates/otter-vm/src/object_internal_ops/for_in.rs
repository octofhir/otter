//! `for-in` key snapshots read from hidden classes (V8's enum cache).
//!
//! # Contents
//! - [`Interpreter::for_in_cached_keys`]: the keys array `ForInKeys` hands
//!   the loop, read from the receiver shape's cache or, for a dictionary
//!   receiver, from its own storage.
//!
//! # Invariants
//! - A shape fixes the string keys, their order and their attributes of every
//!   non-dictionary object without opaque lookup, so those objects'
//!   enumerable keys are a function of the shape: cached once, never changed.
//! - The cache applies only when the receiver and every prototype are such
//!   objects and no prototype has an enumerable string key: the receiver's
//!   own keys are then the whole enumeration (§14.7.5.10). A dictionary
//!   receiver over such prototypes enumerates its own keys the same way,
//!   read fresh since its storage changes without a shape change.
//! - The cached array never reaches script: the loop the compiler emits only
//!   reads it, re-checking each key against the live object.
//! - No object handle is held across an allocation: shapes live in
//!   non-moving old space, and each prototype is re-read from its shape.
//!
//! # See also
//! - `Interpreter::enumerable_for_in_string_keys_for_value` — every other
//!   receiver.

use crate::array::{self, JsArray};
use crate::object::shape_body::{self, ShapeHandle, ShapePrototype};
use crate::object::{self, JsObject};
use crate::{Interpreter, JsString, Value, VmError};

impl Interpreter {
    /// The keys `for (k in target)` visits when no prototype contributes an
    /// enumerable string key: a shaped receiver's from its shape's enum
    /// cache, a dictionary receiver's read from its own storage (V8's
    /// dictionary-mode `KeyAccumulator`). `None` when the receiver or a
    /// prototype needs the general enumeration.
    pub(crate) fn for_in_cached_keys(&mut self, target: Value) -> Result<Option<Value>, VmError> {
        let Some(receiver) = target.as_object() else {
            return Ok(None);
        };
        let Some(dictionary) = self.gc_heap.read_payload(receiver, |body| {
            (!body.chain_link_opaque()).then(|| body.is_dictionary())
        }) else {
            return Ok(None);
        };
        let shape = object::shape(receiver, &self.gc_heap);
        let prototype = if dictionary {
            match object::prototype_value(receiver, &self.gc_heap) {
                None => ShapePrototype::Null,
                Some(prototype) => match prototype.as_object() {
                    Some(holder) => ShapePrototype::Object(holder),
                    None if prototype.is_null() => ShapePrototype::Null,
                    None => return Ok(None),
                },
            }
        } else {
            shape_body::prototype_of(shape)
        };
        if !self.prototypes_enumerate_nothing(prototype)? {
            return Ok(None);
        }
        if dictionary {
            // A dictionary's keys change without a shape change: read them
            // each time, in own-key order, without the general walk's
            // per-key descriptor reads and shadowing set.
            let names: Vec<String> = object::with_properties(receiver, &self.gc_heap, |p| {
                p.enumerable_keys().map(str::to_string).collect()
            });
            return self.scoped_key_strings(&names).map(Some);
        }
        Ok(Some(Value::array(self.shape_for_in_keys(shape)?)))
    }

    /// Whether the chain from `prototype` holds only shaped ordinary
    /// objects without an enumerable string key.
    fn prototypes_enumerate_nothing(
        &mut self,
        mut prototype: ShapePrototype,
    ) -> Result<bool, VmError> {
        for _ in 0..object::PROTO_CHAIN_HARD_CAP {
            let ShapePrototype::Object(holder) = prototype else {
                return Ok(prototype == ShapePrototype::Null);
            };
            let Some(holder_shape) = plain_shape(holder, &self.gc_heap) else {
                return Ok(false);
            };
            if array::len(self.shape_for_in_keys(holder_shape)?, &self.gc_heap) != 0 {
                return Ok(false);
            }
            prototype = shape_body::prototype_of(holder_shape);
        }
        Ok(false)
    }

    /// `shape`'s enumerable string keys as its cached array.
    fn shape_for_in_keys(&mut self, shape: ShapeHandle) -> Result<JsArray, VmError> {
        if let Some(keys) = shape_body::enum_cache_of(shape).as_array() {
            return Ok(keys);
        }
        let mut keys: Vec<Value> = shape_body::shape_enumerable_keys(&self.gc_heap, shape)
            .into_iter()
            .map(|key| Value::string(JsString::from_handle(key, &self.gc_heap)))
            .collect();
        let keys = array::from_values_with_roots(&mut self.gc_heap, &mut keys, &mut |_| {})?;
        shape_body::set_enum_cache(&mut self.gc_heap, shape, Value::array(keys));
        Ok(keys)
    }
}

/// `obj`'s shape when it alone fixes `obj`'s string keys and attributes.
fn plain_shape(obj: JsObject, heap: &otter_gc::GcHeap) -> Option<ShapeHandle> {
    heap.read_payload(obj, |body| {
        (!body.is_dictionary() && !body.chain_link_opaque()).then(|| object::shape(obj, heap))
    })
}
