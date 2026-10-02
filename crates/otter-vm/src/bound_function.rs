//! `BoundFunction` — runtime carrier for `Function.prototype.bind`.
//!
//! Each successful `f.bind(thisArg, ...prefix)` allocates a
//! [`BoundFunctionBody`] capturing the target callable, the bound
//! `this`, and a prefix of arguments. Subsequent calls dispatch
//! through the wrapper and forward to `target` with
//! `this = bound_this` and `prefix ++ caller_args` as the argument
//! list. Bound prefixes live in collector-owned fixed-layout value slabs.
//! Nested binds preserve the original wrapper chain and argument order.
//!
//! # Contents
//! - [`BoundFunctionBody`] — GC payload.
//! - [`BoundFunction`] — `Copy` wrapper handle.
//! - [`BoundFunctionMetadataProperty`] — per-property state (Builtin
//!   / Deleted / Overridden) for the spec `name` and `length` slots.
//! - [`BOUND_FUNCTION_BODY_TYPE_TAG`] — GC body type tag.
//!
//! # Invariants
//! - The callable prefix has C layout; generated dispatch reads no Rust container.
//! - Bound argument slabs are immutable after publication and traced in place.
//! - Construction roots every input in a handle scope across allocations.
//!
//! # See also
//! - [`crate::value_slab`] — shared collector-owned argument storage.
//! - <https://tc39.es/ecma262/#sec-bound-function-exotic-objects>
//! - <https://tc39.es/ecma262/#sec-function.prototype.bind>

use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::SmallVec;

use crate::function_metadata;
use crate::number::NumberValue;
use crate::object::{self, JsObject};
use crate::{Interpreter, Value, VmError};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`BoundFunctionBody`].
pub const BOUND_FUNCTION_BODY_TYPE_TAG: u8 = 0x1c;

/// Own metadata-property state for bound function objects.
#[derive(Debug, Clone)]
pub(crate) enum BoundFunctionMetadataProperty {
    /// The spec-created `name` / `length` property is still present.
    Builtin,
    /// The configurable own property was deleted.
    Deleted,
    /// The property was redefined through `Object.defineProperty`.
    Overridden(object::PropertyDescriptor),
}

impl crate::pelt::PeltField for BoundFunctionMetadataProperty {
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        if let Self::Overridden(desc) = self {
            desc.pelt_trace(visitor);
        }
    }
}

/// GC-allocated storage for `Value::BoundFunction`. Constructed by
/// the `Op::BindFunction` opcode and consumed by every call dispatch
/// path (`Op::Call`, `Op::CallWithThis`, `Op::CallMethodValue`).
#[repr(C)]
#[derive(Debug, Clone, otter_macros::Pelt)]
#[pelt(tag = BOUND_FUNCTION_BODY_TYPE_TAG)]
pub struct BoundFunctionBody {
    /// Underlying callable, including another bound function.
    pub target: Value,
    /// The `this` value the bound call receives. Overrides any
    /// receiver the caller supplies.
    pub bound_this: Value,
    /// Arguments prepended to the caller's argument list at every
    /// invocation. Null denotes an empty prefix.
    pub(crate) bound_args: crate::value_slab::ValueSlabHandle,
    /// Bound function builtin `name`, computed once by `bind`.
    pub(crate) builtin_name: String,
    /// Bound function builtin `length`, computed once by `bind`.
    #[pelt(skip)]
    pub(crate) builtin_length: NumberValue,
    /// Own `name` metadata property state.
    pub(crate) name_property: BoundFunctionMetadataProperty,
    /// Own `length` metadata property state.
    pub(crate) length_property: BoundFunctionMetadataProperty,
    /// Ordinary own properties added after bind creation.
    pub(crate) own_properties: JsObject,
    /// §10.4.1 bound [[Prototype]]: `bind` copies the target's
    /// [[GetPrototypeOf]] result, and a later `Object.setPrototypeOf`
    /// lands here. `None` means the %Function.prototype% default; a
    /// stored `Value::null()` is an explicit null [[Prototype]].
    pub(crate) prototype_override: Option<Value>,
}

/// Byte offset of the target callable in [`BoundFunctionBody`]'s payload.
pub const BOUND_FUNCTION_BODY_TARGET_OFFSET: usize =
    std::mem::offset_of!(BoundFunctionBody, target);
/// Byte offset of the bound `this` in [`BoundFunctionBody`]'s payload.
pub const BOUND_FUNCTION_BODY_THIS_OFFSET: usize =
    std::mem::offset_of!(BoundFunctionBody, bound_this);
/// Byte offset of the compressed bound-argument slab handle (zero when the
/// prefix is empty) in [`BoundFunctionBody`]'s payload.
pub const BOUND_FUNCTION_BODY_ARGS_OFFSET: usize =
    std::mem::offset_of!(BoundFunctionBody, bound_args);

impl BoundFunctionBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        crate::code_liveness::visit_value(&self.target, visitor);
        crate::code_liveness::visit_value(&self.bound_this, visitor);
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
        for property in [&self.name_property, &self.length_property] {
            let BoundFunctionMetadataProperty::Overridden(descriptor) = property else {
                continue;
            };
            crate::code_liveness::visit_descriptor(descriptor, visitor);
        }
    }
}

/// Cheap-to-clone handle for [`BoundFunctionBody`].
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct BoundFunction {
    pub(crate) inner: otter_gc::Gc<BoundFunctionBody>,
}

impl BoundFunction {
    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
    }

    /// Reinterpret a body handle as the public [`BoundFunction`]
    /// wrapper. Used by [`crate::value::Value::as_bound_function`]
    /// after a `GcHeader::type_tag` check has confirmed the body is a
    /// [`BoundFunctionBody`].
    #[inline]
    #[must_use]
    pub fn from_gc(inner: otter_gc::Gc<BoundFunctionBody>) -> Self {
        Self { inner }
    }

    /// Stable identity token.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        self.inner.as_header_ptr() as *const ()
    }

    /// The stored [[Prototype]] override, if any (`Value::null()` is
    /// an explicit null [[Prototype]]).
    #[must_use]
    pub(crate) fn prototype_override(&self, heap: &otter_gc::GcHeap) -> Option<Value> {
        heap.read_payload(self.inner, |body| body.prototype_override)
    }

    /// Store the [[Prototype]] override.
    pub(crate) fn set_prototype_override(&self, heap: &mut otter_gc::GcHeap, proto: Value) {
        heap.with_payload(self.inner, |body| {
            body.prototype_override = Some(proto);
        });
        heap.record_write(self.inner, &proto);
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    /// Clone the callable parts so dispatch can release the heap
    /// borrow before continuing with mutable interpreter work.
    #[must_use]
    pub fn parts(&self, heap: &otter_gc::GcHeap) -> (Value, Value, SmallVec<[Value; 4]>) {
        heap.read_payload(self.inner, |body| {
            let args =
                crate::value_slab::body_of(body.bound_args).map_or_else(SmallVec::new, |slab| {
                    // SAFETY: the bound function retains this slab, and the
                    // immutable initialized prefix stays live for this read.
                    let slab = unsafe { &*slab };
                    SmallVec::from_slice(unsafe {
                        std::slice::from_raw_parts(slab.values_ptr(), slab.len())
                    })
                });
            (body.target, body.bound_this, args)
        })
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = self as *const BoundFunction as *mut RawGc;
        visitor(p);
    }
}

impl Interpreter {
    /// Allocate the fixed-layout bound callable with every input scoped across
    /// property-bag, argument-slab and callable-body allocations.
    pub(crate) fn alloc_bound_function(
        &mut self,
        target: Value,
        bound_this: Value,
        bound_args: &[Value],
        metadata: function_metadata::BoundFunctionCreateMetadata,
        prototype: Option<Value>,
    ) -> Result<BoundFunction, VmError> {
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, target);
            let receiver = interp.scoped_value(scope, bound_this);
            let prototype = prototype.map(|value| interp.scoped_value(scope, value));
            let args: Vec<_> = bound_args
                .iter()
                .copied()
                .map(|value| interp.scoped_value(scope, value))
                .collect();
            let default_prototype = interp.function_prototype_object().ok().map(Value::object);
            let prototype =
                prototype.filter(|value| Some(interp.escape_scoped(*value)) != default_prototype);
            let own_properties =
                object::alloc_dictionary_object_with_roots(&mut interp.gc_heap, &mut |_| {})?;
            let own_properties = interp.scoped_value(scope, Value::object(own_properties));
            let mut values: Vec<_> = args.iter().map(|arg| interp.escape_scoped(*arg)).collect();
            let bound_args =
                crate::value_slab::slab_from_values(&mut interp.gc_heap, &mut values, &mut |_| {})?;
            let body = BoundFunctionBody {
                target: interp.escape_scoped(target),
                bound_this: interp.escape_scoped(receiver),
                bound_args,
                builtin_name: metadata.name,
                builtin_length: metadata.length,
                name_property: BoundFunctionMetadataProperty::Builtin,
                length_property: BoundFunctionMetadataProperty::Builtin,
                own_properties: interp
                    .escape_scoped(own_properties)
                    .as_object()
                    .expect("bound callable property bag"),
                prototype_override: prototype.map(|value| interp.escape_scoped(value)),
            };
            // The allocator traces the pending body, including its slab handle,
            // before publishing it. No allocation separates slab and body setup.
            Ok(BoundFunction {
                inner: interp.gc_heap.alloc(body)?,
            })
        })
    }
}
