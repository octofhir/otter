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

use otter_gc::raw::RawGc;

use crate::Value;
use crate::number::NumberValue;
use crate::object::{
    self, JsObject, MappedArgumentEntry, PartialPropertyDescriptor, PropertyDescriptor,
};
use crate::symbol::JsSymbol;

/// GC root provider covering the arguments object under construction.
///
/// Each `define_own_property` below can allocate (wide-number boxing in
/// `SlotData::into_flat`, dictionary conversion) and therefore move the
/// heap. The `obj` handle, the remaining argv values, the callee /
/// iterator-method values, and the mapped parameter cells must all be
/// forwarded by that collection, or the next loop iteration reads a
/// stale handle into reused memory (observed as shape/dictionary hybrid
/// corruption under `OTTER_GC_STRESS=full`).
struct ArgsInitRoots {
    obj: *mut JsObject,
    args: *mut SmallVec<[Value; 4]>,
    extra: [*mut Value; 2],
    entries: *mut Vec<MappedArgumentEntry>,
}

impl otter_gc::FrameRoots for ArgsInitRoots {
    fn trace(&self, visitor: &mut dyn FnMut(*mut RawGc)) {
        // SAFETY: the pointed-at locals outlive the provider registration —
        // it is popped before `initialize_*` returns, and GC tracing runs
        // synchronously during the pause.
        unsafe {
            visitor(self.obj.cast::<RawGc>());
            for value in (*self.args).iter_mut() {
                value.trace_value_slot_mut(visitor);
            }
            for &extra in &self.extra {
                if !extra.is_null() {
                    (*extra).trace_value_slot_mut(visitor);
                }
            }
            if !self.entries.is_null() {
                for entry in (*self.entries).iter_mut() {
                    visitor((&mut entry.cell as *mut crate::upvalue::UpvalueCell).cast::<RawGc>());
                }
            }
        }
    }
}

/// Populate an unmapped `arguments` object from a captured argv list.
pub(crate) fn initialize_unmapped(
    mut obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    mut args: SmallVec<[Value; 4]>,
    mut throw_type_error: Value,
    iterator: Option<(JsSymbol, Value)>,
) {
    let (iterator_symbol, mut iterator_method) = match iterator {
        Some((symbol, method)) => (Some(symbol), method),
        None => (None, Value::undefined()),
    };
    let roots = ArgsInitRoots {
        obj: &mut obj,
        args: &mut args,
        extra: [&mut throw_type_error, &mut iterator_method],
        entries: std::ptr::null_mut(),
    };
    let depth = heap.push_frame_roots(&roots as *const dyn otter_gc::FrameRoots) - 1;

    object::mark_as_arguments_object(obj, heap);
    for index in 0..args.len() {
        let key = index.to_string();
        let descriptor = PropertyDescriptor::data(args[index], true, true, true);
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
    if let Some(symbol) = iterator_symbol {
        object::define_own_symbol_property_partial(
            obj,
            heap,
            symbol,
            PartialPropertyDescriptor {
                value: Some(iterator_method),
                writable: Some(true),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
    }
    heap.pop_frame_roots_to(depth);
}

/// Populate a sloppy mapped `arguments` object from a captured argv
/// list plus VM-internal parameter cells.
pub(crate) fn initialize_mapped(
    mut obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    mut args: SmallVec<[Value; 4]>,
    mut callee: Value,
    mut mapped_entries: Vec<MappedArgumentEntry>,
    iterator: Option<(JsSymbol, Value)>,
) {
    let (iterator_symbol, mut iterator_method) = match iterator {
        Some((symbol, method)) => (Some(symbol), method),
        None => (None, Value::undefined()),
    };
    let roots = ArgsInitRoots {
        obj: &mut obj,
        args: &mut args,
        extra: [&mut callee, &mut iterator_method],
        entries: &mut mapped_entries,
    };
    let depth = heap.push_frame_roots(&roots as *const dyn otter_gc::FrameRoots) - 1;

    object::mark_as_arguments_object(obj, heap);
    for index in 0..args.len() {
        let key = index.to_string();
        let descriptor = PropertyDescriptor::data(args[index], true, true, true);
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
    if let Some(symbol) = iterator_symbol {
        object::define_own_symbol_property_partial(
            obj,
            heap,
            symbol,
            PartialPropertyDescriptor {
                value: Some(iterator_method),
                writable: Some(true),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
    }
    // Move the entries out through the same slot the provider traces:
    // `mem::take` leaves a valid empty Vec behind, so a collection during
    // `install_mapped_arguments` still traces coherent state (obj included).
    let entries = std::mem::take(&mut mapped_entries);
    object::install_mapped_arguments(obj, heap, entries);
    heap.pop_frame_roots_to(depth);
}
