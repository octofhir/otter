//! The `prototype` property of ordinary functions, held in a dedicated slot.
//!
//! # Contents
//! - [`Interpreter::function_prototype_slot`] — the slot's value and
//!   writability for a function kind that owns `prototype`.
//! - [`Interpreter::function_prototype_value`] — `[[Get]]` of the slot,
//!   allocating the default prototype object on first use.
//! - [`Interpreter::store_function_prototype`] /
//!   [`Interpreter::freeze_function_prototype`] — the slot's only mutations.
//!
//! # Invariants
//! - Like V8's `JSFunction::prototype_or_initial_map`, the property lives in
//!   the closure's rare record (a bare function value's in a side table),
//!   never in the own-property bag: assigning `F.prototype` gives a closure
//!   no bag, so its named lookups stay ordinary and generated `F.call`,
//!   construct and `instanceof` proofs read one slot.
//! - The property exists from creation for every kind
//!   [`crate::ExecutionContext::function_has_prototype_property`] admits:
//!   `{writable: true, enumerable: false, configurable: false}`. The value is
//!   the hole until the default object is allocated, exactly once. Only the
//!   writable attribute changes, and only from true to false.
//! - Functions whose kind has no `prototype` keep any user-created
//!   `prototype` property in the bag like every other own property.
//!
//! # See also
//! - `crate::closure_construct` — the rare record's layout.
//! - `crate::function_ops` — the ordinary function property operations.

use crate::{ActivationStack, ExecutionContext, Interpreter, JsObject, Value, VmError, object};

impl Interpreter {
    /// The `prototype` slot of a function whose kind owns one: its value (the
    /// hole until the default object is allocated) and whether it is
    /// writable. `None` for a kind with no `prototype` property.
    pub(crate) fn function_prototype_slot(
        &self,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
    ) -> Option<(Value, bool)> {
        let owner_context = context.for_function(function_id).ok()?;
        if !owner_context.function_has_prototype_property(function_id) {
            return None;
        }
        Some(match owner {
            Some(closure) => closure
                .prototype_slot(&self.gc_heap)
                .unwrap_or((Value::hole(), true)),
            None => self
                .function_prototype_slots
                .get(&function_id)
                .copied()
                .unwrap_or((Value::hole(), true)),
        })
    }

    /// Store `value` into the `prototype` slot. The caller has checked the
    /// slot exists and is writable. A closure gets its rare record here; the
    /// allocation roots `value` and the closure.
    pub(crate) fn store_function_prototype(
        &mut self,
        stack: Option<&ActivationStack>,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        value: Value,
    ) -> Result<(), VmError> {
        match owner {
            Some(closure) => self.with_handle_scope(|interp, scope| {
                // The handle arena roots the value across the rare-record
                // allocation; the closure slot is rewritten in place.
                let value = interp.scoped_value(scope, value);
                let mut closure_value = Value::closure(closure);
                interp.ensure_closure_rare(stack, &mut closure_value, &[])?;
                let value = interp.escape_scoped(value);
                let closure = closure_value
                    .as_closure(&interp.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                interp.retire_instanceof_proofs_for(closure);
                closure.set_prototype_value(&mut interp.gc_heap, value);
                Ok::<(), VmError>(())
            })?,
            None => {
                let writable = self
                    .function_prototype_slots
                    .get(&function_id)
                    .is_none_or(|&(_, writable)| writable);
                self.function_prototype_slots
                    .insert(function_id, (value, writable));
            }
        }
        Ok(())
    }

    /// Clear the `prototype` slot's writable attribute. The caller has
    /// allocated the default object first, so the frozen slot holds a value.
    pub(crate) fn freeze_function_prototype(
        &mut self,
        stack: Option<&ActivationStack>,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
    ) -> Result<(), VmError> {
        match owner {
            Some(closure) => {
                let mut closure_value = Value::closure(closure);
                self.ensure_closure_rare(stack, &mut closure_value, &[])?;
                closure_value
                    .as_closure(&self.gc_heap)
                    .ok_or(VmError::TypeMismatch)?
                    .freeze_prototype(&mut self.gc_heap);
            }
            None => {
                let value = self
                    .function_prototype_slots
                    .get(&function_id)
                    .map_or(Value::hole(), |&(value, _)| value);
                self.function_prototype_slots
                    .insert(function_id, (value, false));
            }
        }
        Ok(())
    }

    /// `[[Get]]` of the `prototype` slot: its value, allocating the default
    /// prototype object on first use (§10.2.5 MakeConstructor, §27.3.4 /
    /// §27.4.4 for generator kinds). `receiver` is the canonical callable that
    /// the default object's `constructor` names. The caller has checked that
    /// the kind owns a `prototype`.
    pub(crate) fn function_prototype_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        owner: Option<crate::closure::JsClosure>,
        function_id: u32,
        receiver: Option<Value>,
    ) -> Result<Value, VmError> {
        if let Some((value, _)) = self.function_prototype_slot(context, owner, function_id)
            && !value.is_hole()
        {
            return Ok(value);
        }
        // Every allocation below may move the closure and the new prototype,
        // so each lives in a handle and is re-read after every step.
        self.with_handle_scope(|interp, scope| -> Result<Value, VmError> {
            let function_root = Value::function(function_id);
            let constructor_handle = interp.scoped_value(scope, receiver.unwrap_or(function_root));
            let owner_handle = owner.map(|owner| interp.scoped_value(scope, Value::closure(owner)));
            let owner_now = |interp: &Self| {
                owner_handle
                    .and_then(|handle| interp.escape_scoped(handle).as_closure(&interp.gc_heap))
            };
            let proto = interp.alloc_stack_rooted_object_with_extra_roots(stack, &[])?;
            let proto_handle = interp.scoped_value(scope, Value::object(proto));
            let proto_now = |interp: &Self| -> JsObject {
                interp
                    .escape_scoped(proto_handle)
                    .as_object()
                    .expect("prototype handle holds an object")
            };
            if let Some(object_proto) = interp.realm_intrinsics.object_prototype().or_else(|| {
                object::get(interp.global_this, &interp.gc_heap, "Object")
                    .and_then(|v| v.as_object())
                    .and_then(|object_ctor| {
                        object::get(object_ctor, &interp.gc_heap, "prototype")
                            .and_then(|v| v.as_object())
                    })
            }) {
                let mut prototype = proto_now(interp);
                if !object::set_prototype(&mut prototype, &mut interp.gc_heap, Some(object_proto))?
                {
                    return Err(VmError::TypeError);
                }
            }
            let is_generator = context
                .function(function_id)
                .is_some_and(|function| function.is_generator);
            if is_generator {
                let is_async = context
                    .function(function_id)
                    .is_some_and(|function| function.is_async_generator);
                if let Some(shared) = interp.shared_generator_object_prototype(is_async) {
                    // §27.5.1 / §27.6.1 — generator-function `.prototype`
                    // objects inherit from the one shared
                    // %GeneratorPrototype% / %AsyncGeneratorPrototype%.
                    let mut prototype = proto_now(interp);
                    if !object::set_prototype(&mut prototype, &mut interp.gc_heap, Some(shared))? {
                        return Err(VmError::TypeError);
                    }
                } else {
                    let parent = interp.alloc_stack_rooted_object_with_extra_roots(stack, &[])?;
                    let proto = proto_now(interp);
                    interp.finish_generator_function_prototype(
                        context,
                        function_id,
                        proto,
                        parent,
                    )?;
                }
            }
            // §27.5.1 — a generator function's `.prototype` object has NO own
            // properties (no back-pointing `constructor`); ordinary functions
            // get the §10.2.5 MakeConstructor pair. The install advances the
            // hidden class, so the prototype keeps a fast shape and
            // prototype-style method definitions land in shape slots.
            if !is_generator {
                let constructor_desc = object::PartialPropertyDescriptor {
                    value: Some(interp.escape_scoped(constructor_handle)),
                    writable: Some(true),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                };
                let mut proto = proto_now(interp);
                let _ = interp.define_own_property_partial(
                    &mut proto,
                    "constructor",
                    constructor_desc,
                )?;
            }
            let value = Value::object(proto_now(interp));
            interp.store_function_prototype(Some(stack), owner_now(interp), function_id, value)?;
            Ok(interp.escape_scoped(proto_handle))
        })
    }
}
