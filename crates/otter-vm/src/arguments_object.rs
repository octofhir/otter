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

use otter_gc::RootScope;
use otter_gc::raw::RawGc;

use crate::Value;
use crate::number::NumberValue;
use crate::object::{self, JsObject, MappedArgumentEntry, PartialPropertyDescriptor};
use crate::rooting::RootScopeExt;
use crate::symbol::JsSymbol;

/// GC-safety: each `define_own_property` below can allocate (wide-number
/// boxing in `SlotData::into_flat`, dictionary conversion) and move the
/// heap, so the object handle, remaining argv values, callee / iterator
/// method, and parameter-map cells are all registered on a
/// [`otter_gc::RootScope`] for the whole initialization; a collection
/// forwards them in place.
/// Populate an unmapped `arguments` object from a captured argv list.
pub(crate) fn initialize_unmapped(
    mut obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    mut args: SmallVec<[Value; 4]>,
    mut throw_type_error: Value,
    iterator: Option<(JsSymbol, Value)>,
    shape: crate::object::ShapeHandle,
) -> JsObject {
    let (iterator_symbol, mut iterator_method) = match iterator {
        Some((symbol, method)) => (Some(symbol), method),
        None => (None, Value::undefined()),
    };
    let mut scope = RootScope::new(heap);
    // SAFETY: the rooted locals are declared above the scope and outlive it.
    unsafe {
        scope.add_object(&mut obj);
        scope.add_value_smallvec(&mut args);
        scope.add_value(&mut throw_type_error);
        scope.add_value(&mut iterator_method);
    }

    object::mark_as_arguments_object(&mut obj, heap);
    // The shape already names every slot: indices, then `length`, then the
    // `callee` accessor. One shape install plus a flat slab fill replaces a
    // dictionary insert per key.
    let callee_cell = object::alloc_accessor_cell(
        heap,
        &mut obj,
        Some(throw_type_error),
        Some(throw_type_error),
    )
    .expect("arguments callee accessor cell allocation");
    let mut slab: SmallVec<[Value; 8]> = SmallVec::with_capacity(args.len() + 2);
    slab.extend(args.iter().copied());
    slab.push(Value::number(NumberValue::from_i32(args.len() as i32)));
    slab.push(callee_cell);
    object::set_fresh_object_shape(obj, heap, shape);
    object::initialize_shaped_data_slots(obj, heap, &slab);
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
    drop(scope);
    // `obj` was forwarded in place by the rooted scope across the
    // property definitions above; hand the current handle back so the
    // caller writes the live object into its register, not a stale copy.
    obj
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
    shape: crate::object::ShapeHandle,
) -> JsObject {
    let (iterator_symbol, mut iterator_method) = match iterator {
        Some((symbol, method)) => (Some(symbol), method),
        None => (None, Value::undefined()),
    };
    let mut scope = RootScope::new(heap);
    // SAFETY: the rooted locals are declared above the scope and outlive it.
    unsafe {
        scope.add_object(&mut obj);
        scope.add_value_smallvec(&mut args);
        scope.add_value(&mut callee);
        scope.add_value(&mut iterator_method);
        for entry in mapped_entries.iter_mut() {
            scope.add_raw_slot(
                (&mut entry.cell as *mut crate::upvalue::UpvalueCell).cast::<RawGc>(),
            );
        }
    }

    object::mark_as_arguments_object(&mut obj, heap);
    // The shape already names every slot: indices, then `length`, then
    // `callee`. One shape install plus a flat slab fill replaces a dictionary
    // insert per key.
    let mut slab: SmallVec<[Value; 8]> = SmallVec::with_capacity(args.len() + 2);
    slab.extend(args.iter().copied());
    slab.push(Value::number(NumberValue::from_i32(args.len() as i32)));
    slab.push(callee);
    object::set_fresh_object_shape(obj, heap, shape);
    object::initialize_shaped_data_slots(obj, heap, &slab);
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
    // The entry cells are rooted as individual slots (stable Vec storage:
    // the vector is not resized while the scope is open), so the vector can
    // move into `install_mapped_arguments` only after the scope closes; by
    // then `obj` and the cells are current.
    drop(scope);
    object::install_mapped_arguments(obj, heap, mapped_entries);
    obj
}
