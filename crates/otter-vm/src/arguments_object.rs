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
//! - [`allocate`] — one unmapped or mapped arguments object, its slots
//!   filled by its allocation.
//!
//! # Invariants
//! - The object is represented with the ordinary descriptor-capable
//!   object storage; no array identity is exposed.
//! - The final shape names every own property, `@@iterator` last, so the
//!   object's one allocation fills it; no empty object is ever reachable.
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

/// Allocate an arguments object of the final `shape` with every slot it
/// names already filled: `elements`, `length`, `callee` (the realm's shared
/// `%ThrowTypeError%` accessor cell when unmapped, the function when mapped),
/// then `@@iterator` when `iterator` is given. The object is marked as an
/// arguments exotic and, with `mapped`, given its ParameterMap.
///
/// One allocation publishes the object with its values, as an object
/// literal's is. The marker sidecar and the ParameterMap allocate after it;
/// the object and the parameter-scope context ride a root scope across them.
/// `external_visit` keeps the caller's own pending values current.
pub(crate) fn allocate(
    heap: &mut otter_gc::GcHeap,
    shape: object::ShapeHandle,
    elements: &[Value],
    callee: Value,
    iterator: Option<Value>,
    mut mapped: Option<MappedArguments>,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let mut values: SmallVec<[Value; 8]> = SmallVec::with_capacity(elements.len() + 3);
    values.extend(elements.iter().copied());
    values.push(Value::number(NumberValue::from_i32(elements.len() as i32)));
    values.push(callee);
    values.extend(iterator);
    let context_slot = mapped
        .as_mut()
        .map(|mapped| (&mut mapped.context as *mut crate::context::ContextHandle).cast::<RawGc>());
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        if let Some(slot) = context_slot {
            visitor(slot);
        }
    };
    let mut obj = object::alloc_object_with_shape_and_values_roots(
        heap,
        shape,
        &mut values,
        otter_gc::SitePlacement::Young(0),
        &mut visit,
    )?;
    let mut scope = RootScope::new(heap);
    // SAFETY: the object handle and the context slot precede the scope and
    // remain stationary until it closes.
    unsafe {
        scope.add_object(&mut obj);
        if let Some(slot) = context_slot {
            scope.add_raw_slot(slot);
        }
    }
    object::mark_as_arguments_object(&mut obj, heap)?;
    // The context is rooted in place, so the map moves into
    // `install_mapped_arguments` only after the scope closes; by then `obj`
    // and the context are current.
    drop(scope);
    if let Some(mapped) = mapped {
        object::install_mapped_arguments(&mut obj, heap, mapped)?;
    }
    Ok(obj)
}
