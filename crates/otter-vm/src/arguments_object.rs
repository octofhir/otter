//! Unmapped `arguments` object construction.
//!
//! This module builds the strict-mode / unmapped arguments object
//! shape used by [`otter_bytecode::Op::CollectArguments`]. It is a
//! focused implementation of ECMA-262 §10.4.4.6: indexed own data
//! properties, a non-enumerable `length`, and a restricted `callee`
//! accessor using the realm's shared `%ThrowTypeError%` function.
//!
//! # Contents
//! - [`create_unmapped`] — allocate and populate an arguments object.
//!
//! # Invariants
//! - The object is represented with the ordinary descriptor-capable
//!   object storage; no array identity is exposed.
//! - Indexed properties are writable, enumerable, and configurable.
//! - `length` is writable, non-enumerable, and configurable.
//! - `callee` is an accessor with `[[Configurable]]: false`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-createunmappedargumentsobject>

use smallvec::SmallVec;

use crate::Value;
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor};

/// Allocate an unmapped `arguments` object from a captured argv list.
pub(crate) fn create_unmapped(
    heap: &mut otter_gc::GcHeap,
    args: SmallVec<[Value; 4]>,
    throw_type_error: Value,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let obj = object::alloc_object(heap)?;
    for (index, value) in args.iter().cloned().enumerate() {
        let key = index.to_string();
        let descriptor = PropertyDescriptor::data(value, true, true, true);
        object::define_own_property(obj, heap, &key, descriptor);
    }

    let length = Value::Number(NumberValue::from_i32(args.len() as i32));
    object::define_own_property(
        obj,
        heap,
        "length",
        PropertyDescriptor::data(length, true, false, true),
    );
    object::define_own_property(
        obj,
        heap,
        "callee",
        PropertyDescriptor::accessor(
            Some(throw_type_error.clone()),
            Some(throw_type_error),
            false,
            false,
        ),
    );
    Ok(obj)
}
