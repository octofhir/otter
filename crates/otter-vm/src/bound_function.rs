//! `BoundFunction` — runtime carrier for `Function.prototype.bind`.
//!
//! Each successful `f.bind(thisArg, ...prefix)` allocates a
//! [`BoundFunctionBody`] capturing the target callable, the bound
//! `this`, and a prefix of arguments. Subsequent calls dispatch
//! through the wrapper and forward to `target` with
//! `this = bound_this` and `prefix ++ caller_args` as the argument
//! list. Chained `bind` flattens by re-wrapping at call time without
//! unbounded recursion (one hop per layer).
//!
//! # Contents
//! - [`BoundFunctionBody`] — GC payload.
//! - [`BoundFunction`] — `Copy` wrapper handle.
//! - [`BoundFunctionMetadataProperty`] — per-property state (Builtin
//!   / Deleted / Overridden) for the spec `name` and `length` slots.
//! - [`BOUND_FUNCTION_BODY_TYPE_TAG`] — GC body type tag.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-bound-function-exotic-objects>
//! - <https://tc39.es/ecma262/#sec-function.prototype.bind>

use otter_gc::raw::{RawGc, SlotVisitor};
use smallvec::SmallVec;

use crate::Value;
use crate::function_metadata;
use crate::number::NumberValue;
use crate::object::{self, JsObject};

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

/// GC-allocated storage for `Value::BoundFunction`. Constructed by
/// the `Op::BindFunction` opcode and consumed by every call dispatch
/// path (`Op::Call`, `Op::CallWithThis`, `Op::CallMethodValue`).
#[derive(Debug, Clone)]
pub struct BoundFunctionBody {
    /// Underlying callable. Foundation slice keeps this as a `Value`;
    /// chained `bind` flattens by re-wrapping at call time without
    /// unbounded recursion (one hop per layer).
    pub target: Value,
    /// The `this` value the bound call receives. Overrides any
    /// receiver the caller supplies.
    pub bound_this: Value,
    /// Arguments prepended to the caller's argument list at every
    /// invocation. Stored inline up to four entries to keep the usual
    /// `f.bind(t, a, b)` shape off the heap.
    pub(crate) bound_args: SmallVec<[Value; 4]>,
    /// Bound function builtin `name`, computed once by `bind`.
    pub(crate) builtin_name: String,
    /// Bound function builtin `length`, computed once by `bind`.
    pub(crate) builtin_length: NumberValue,
    /// Own `name` metadata property state.
    pub(crate) name_property: BoundFunctionMetadataProperty,
    /// Own `length` metadata property state.
    pub(crate) length_property: BoundFunctionMetadataProperty,
    /// Ordinary own properties added after bind creation.
    pub(crate) own_properties: JsObject,
}

impl otter_gc::SafeTraceable for BoundFunctionBody {
    const TYPE_TAG: u8 = BOUND_FUNCTION_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        self.target.trace_value_slots(visitor);
        self.bound_this.trace_value_slots(visitor);
        for arg in &self.bound_args {
            arg.trace_value_slots(visitor);
        }
        trace_bound_metadata_property(&self.name_property, visitor);
        trace_bound_metadata_property(&self.length_property, visitor);
        let p = &self.own_properties as *const JsObject as *mut RawGc;
        visitor(p);
    }
}

fn trace_bound_metadata_property(
    property: &BoundFunctionMetadataProperty,
    visitor: &mut SlotVisitor<'_>,
) {
    let BoundFunctionMetadataProperty::Overridden(desc) = property else {
        return;
    };
    match &desc.kind {
        object::DescriptorKind::Data { value } => value.trace_value_slots(visitor),
        object::DescriptorKind::Accessor { getter, setter } => {
            if let Some(getter) = getter {
                getter.trace_value_slots(visitor);
            }
            if let Some(setter) = setter {
                setter.trace_value_slots(visitor);
            }
        }
    }
}

fn no_extra_roots(_: &mut dyn FnMut(*mut RawGc)) {}

/// Cheap-to-clone handle for [`BoundFunctionBody`].
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct BoundFunction {
    pub(crate) inner: otter_gc::Gc<BoundFunctionBody>,
}

impl BoundFunction {
    /// Allocate a bound-function body on the GC heap.
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Self::new_with_metadata(
            heap,
            target,
            bound_this,
            bound_args,
            function_metadata::BoundFunctionCreateMetadata {
                name: "bound ".to_string(),
                length: NumberValue::from_i32(0),
            },
        )
    }

    /// Build a bound function with spec-computed `name` / `length`
    /// metadata captured at bind time.
    pub(crate) fn new_with_metadata(
        heap: &mut otter_gc::GcHeap,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        metadata: function_metadata::BoundFunctionCreateMetadata,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let mut external_visit = no_extra_roots;
        Self::new_with_metadata_and_roots(
            heap,
            target,
            bound_this,
            bound_args,
            metadata,
            &mut external_visit,
        )
    }

    /// Build a bound function while exposing caller-owned roots
    /// across the function's ordinary property bag and body
    /// allocations.
    pub(crate) fn new_with_metadata_and_roots(
        heap: &mut otter_gc::GcHeap,
        target: Value,
        bound_this: Value,
        bound_args: SmallVec<[Value; 4]>,
        metadata: function_metadata::BoundFunctionCreateMetadata,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let own_properties = {
            let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                target.trace_value_slots(visitor);
                bound_this.trace_value_slots(visitor);
                for arg in &bound_args {
                    arg.trace_value_slots(visitor);
                }
            };
            object::alloc_object_with_roots(heap, &mut visit)?
        };
        let own_properties_root = Value::object(own_properties);
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            own_properties_root.trace_value_slots(visitor);
            target.trace_value_slots(visitor);
            bound_this.trace_value_slots(visitor);
            for arg in &bound_args {
                arg.trace_value_slots(visitor);
            }
        };
        Ok(Self {
            inner: heap.alloc_with_roots(
                BoundFunctionBody {
                    target,
                    bound_this,
                    bound_args: bound_args.clone(),
                    builtin_name: metadata.name,
                    builtin_length: metadata.length,
                    name_property: BoundFunctionMetadataProperty::Builtin,
                    length_property: BoundFunctionMetadataProperty::Builtin,
                    own_properties,
                },
                &mut visit,
            )?,
        })
    }

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
            (body.target, body.bound_this, body.bound_args.clone())
        })
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = self as *const BoundFunction as *mut RawGc;
        visitor(p);
    }
}
