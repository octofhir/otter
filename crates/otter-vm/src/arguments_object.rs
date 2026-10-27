//! `arguments` object construction.
//!
//! This module builds the strict-mode / unmapped and sloppy mapped
//! arguments object shapes used by
//! [`otter_bytecode::Op::CollectArguments`]. The unmapped variant
//! follows ECMA-262 §10.4.4.6: indexed own data properties, a
//! non-enumerable `length`, and a restricted `callee` accessor using
//! the realm's shared `%ThrowTypeError%` function in one accessor cell all
//! of the realm's unmapped objects share. The mapped variant
//! adds a VM-internal ParameterMap over selected indexed properties: each
//! mapped index aliases a slot of the parameter-scope context.
//!
//! # Contents
//! - [`ArgumentsShapeKind`] — the variant a cached final shape stands for.
//! - [`initialize_unmapped`] — populate an unmapped arguments object.
//! - [`initialize_mapped`] — populate a sloppy mapped arguments object.
//!
//! # Invariants
//! - The object is represented with the ordinary descriptor-capable
//!   object storage; no array identity is exposed.
//! - The caller allocates the final shape's lineage root and inline capacity
//!   before initialization; installing its slots preserves that footprint.
//! - The final shape names every own property, `@@iterator` last, so one
//!   shape install fills the object.
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
use crate::object::{self, JsObject, MappedArguments};
use crate::rooting::RootScopeExt;

/// Which final hidden class of one arity an arguments object takes: mapped
/// or not, whether its lineage carries the ParameterMap lookup fact, and
/// whether it owns `@@iterator` (absent only before the realm has
/// `%Array.prototype.values%`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ArgumentsShapeKind(u8);

impl ArgumentsShapeKind {
    pub(crate) const fn new(mapped: bool, parameter_map: bool, iterator: bool) -> Self {
        Self(mapped as u8 | (parameter_map as u8) << 1 | (iterator as u8) << 2)
    }
}

/// GC-safety: the sidecar, slot and accessor allocations below can move the
/// heap, so the object handle, argv values, callee / iterator method, and
/// the parameter-scope context are all registered on a
/// [`otter_gc::RootScope`] for the whole initialization; a collection
/// forwards them in place.
/// Populate an unmapped `arguments` object from a captured argv list;
/// `callee_cell` is the realm's shared `%ThrowTypeError%` accessor pair.
pub(crate) fn initialize_unmapped(
    mut obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    mut args: SmallVec<[Value; 4]>,
    mut callee_cell: Value,
    iterator: Option<Value>,
    shape: crate::object::ShapeHandle,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let has_iterator = iterator.is_some();
    let mut iterator_method = iterator.unwrap_or(Value::undefined());
    let mut scope = RootScope::new(heap);
    // SAFETY: the rooted locals are declared above the scope and outlive it.
    unsafe {
        scope.add_object(&mut obj);
        scope.add_value_smallvec(&mut args);
        scope.add_value(&mut callee_cell);
        scope.add_value(&mut iterator_method);
    }

    object::mark_as_arguments_object(&mut obj, heap)?;
    // Reserve before copying argv/accessor words into the installation buffer.
    // Every pending value still resides in the actual RootScope slots above.
    object::reserve_fresh_object_slot_capacity(&mut obj, heap, args.len() + 3)?;
    // The shape already names every slot: indices, `length`, the `callee`
    // accessor, then `@@iterator`. One shape install plus a flat slab fill
    // replaces a dictionary insert per key.
    let mut slab: SmallVec<[Value; 8]> = SmallVec::with_capacity(args.len() + 3);
    slab.extend(args.iter().copied());
    slab.push(Value::number(NumberValue::from_i32(args.len() as i32)));
    slab.push(callee_cell);
    if has_iterator {
        slab.push(iterator_method);
    }
    object::install_fresh_shape_with_slots(obj, heap, shape, &slab, slab.len());
    drop(scope);
    // `obj` was forwarded in place by the rooted scope across the
    // property definitions above; hand the current handle back so the
    // caller writes the live object into its register, not a stale copy.
    Ok(obj)
}

/// Populate a sloppy mapped `arguments` object from a captured argv
/// list plus its VM-internal ParameterMap over the parameter-scope context.
pub(crate) fn initialize_mapped(
    mut obj: JsObject,
    heap: &mut otter_gc::GcHeap,
    mut args: SmallVec<[Value; 4]>,
    mut callee: Value,
    mut mapped: Option<MappedArguments>,
    iterator: Option<Value>,
    shape: crate::object::ShapeHandle,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let has_iterator = iterator.is_some();
    let mut iterator_method = iterator.unwrap_or(Value::undefined());
    let mut scope = RootScope::new(heap);
    // SAFETY: the rooted locals are declared above the scope and outlive it.
    unsafe {
        scope.add_object(&mut obj);
        scope.add_value_smallvec(&mut args);
        scope.add_value(&mut callee);
        scope.add_value(&mut iterator_method);
        if let Some(mapped) = &mut mapped {
            scope.add_raw_slot(
                (&mut mapped.context as *mut crate::context::ContextHandle).cast::<RawGc>(),
            );
        }
    }

    object::mark_as_arguments_object(&mut obj, heap)?;
    // Reserve before copying argv/accessor words into the installation buffer.
    // Every pending value still resides in the actual RootScope slots above.
    object::reserve_fresh_object_slot_capacity(&mut obj, heap, args.len() + 3)?;
    // The shape already names every slot: indices, `length`, `callee`, then
    // `@@iterator`. One shape install plus a flat slab fill replaces a
    // dictionary insert per key.
    let mut slab: SmallVec<[Value; 8]> = SmallVec::with_capacity(args.len() + 3);
    slab.extend(args.iter().copied());
    slab.push(Value::number(NumberValue::from_i32(args.len() as i32)));
    slab.push(callee);
    if has_iterator {
        slab.push(iterator_method);
    }
    object::install_fresh_shape_with_slots(obj, heap, shape, &slab, slab.len());
    // The context is rooted in place, so the map moves into
    // `install_mapped_arguments` only after the scope closes; by then `obj`
    // and the context are current.
    drop(scope);
    if let Some(mapped) = mapped {
        object::install_mapped_arguments(&mut obj, heap, mapped)?;
    }
    Ok(obj)
}
