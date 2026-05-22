//! `arguments` object construction.
//!
//! This module builds the strict-mode / unmapped and sloppy mapped
//! arguments object shapes used by
//! [`otter_bytecode::Op::CollectArguments`]. The unmapped variant
//! follows ECMA-262 §10.4.4.6: indexed own data properties, a
//! non-enumerable `length`, and a restricted `callee` accessor using
//! the realm's shared `%ThrowTypeError%` function. The mapped variant
//! adds a VM-internal ParameterMap over selected indexed properties.
//!
//! # Contents
//! - [`initialize_unmapped`] — populate an unmapped arguments object.
//! - [`initialize_mapped`] — populate a sloppy mapped arguments object.
//!
//! # Invariants
//! - The object is represented with the ordinary descriptor-capable
//!   object storage; no array identity is exposed.
//! - Indexed properties are writable, enumerable, and configurable.
//! - `length` is writable, non-enumerable, and configurable.
//! - Unmapped `callee` is the restricted accessor with
//!   `[[Configurable]]: false`; mapped sloppy `callee` is an
//!   ordinary writable, non-enumerable, configurable data property.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-createunmappedargumentsobject>

use smallvec::SmallVec;

use crate::Value;
use crate::number::NumberValue;
use crate::object::{self, JsObject, MappedArgumentEntry, PropertyDescriptor};

/// Populate an unmapped `arguments` object from a captured argv list.
pub(crate) fn initialize_unmapped(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    args: SmallVec<[Value; 4]>,
    throw_type_error: Value,
) {
    object::mark_as_arguments_object(obj, heap);
    for (index, value) in args.iter().cloned().enumerate() {
        let key = index.to_string();
        let descriptor = PropertyDescriptor::data(value, true, true, true);
        object::define_own_property(obj, heap, &key, descriptor);
    }

    let length = Value::number(NumberValue::from_i32(args.len() as i32));
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
        PropertyDescriptor::accessor(Some(throw_type_error), Some(throw_type_error), false, false),
    );
}

/// Populate a sloppy mapped `arguments` object from a captured argv
/// list plus VM-internal parameter cells.
pub(crate) fn initialize_mapped(
    obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    args: SmallVec<[Value; 4]>,
    callee: Value,
    mapped_entries: Vec<MappedArgumentEntry>,
) {
    object::mark_as_arguments_object(obj, heap);
    for (index, value) in args.iter().cloned().enumerate() {
        let key = index.to_string();
        let descriptor = PropertyDescriptor::data(value, true, true, true);
        object::define_own_property(obj, heap, &key, descriptor);
    }

    let length = Value::number(NumberValue::from_i32(args.len() as i32));
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
        PropertyDescriptor::data(callee, true, false, true),
    );
    object::install_mapped_arguments(obj, heap, mapped_entries);
}
