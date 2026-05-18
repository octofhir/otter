//! `Value::NativeFunction` — host-implemented callable values.
//!
//! Native callables are GC-managed handles. Production builtins use
//! a static function-pointer dispatch path; dynamic closures remain
//! available for host/embedder cases that need captured Rust state.
//! Any JS values a dynamic closure owns must also be listed in the
//! body's capture list so tracing can keep those values alive.
//!
//! # Contents
//! - [`NativeFunction`] — cheap-to-clone GC handle.
//! - [`NativeFunctionBody`] — name, closure payload, and traced
//!   captured values.
//! - [`NativeFastFn`] / [`NativeCall`] — static and dynamic native
//!   dispatch targets.
//! - [`NativeFn`] — the dynamic closure signature.
//! - [`NativeError`] — failure outcome the dispatcher converts to
//!   `VmError`.
//!
//! # Invariants
//! - Every allocation receives an explicit [`otter_gc::GcHeap`]; active
//!   VM-owned dynamic closures use the root-aware helper when caller roots are
//!   available.
//! - The call signature receives an explicit [`crate::NativeCtx`].
//!   Host async work must copy owned, non-GC data out before any
//!   `.await`; `NativeCtx`, `Value`, and GC handles are
//!   isolate-local.
//! - Static builtins carry a plain function pointer and no captured
//!   payload.
//! - Public dynamic native constructors require `Send + Sync`
//!   closures and pass traced JS captures as an explicit slice at
//!   call time. That keeps embedders from hiding isolate-local
//!   `Gc<T>` / `Value` handles inside a long-lived closure.
//! - Crate-internal unchecked constructors are reserved for audited
//!   isolate-local VM helpers whose payload-specific trace hook covers
//!   every hidden JS value.
//!
//! # See also
//! - [GC API](../../../docs/book/src/engine/gc-api.md)
//! - [Native bindings](../../../docs/book/src/extensions/native-bindings.md)

use std::rc::Rc;
use std::sync::Arc;

use smallvec::SmallVec;

use crate::object::{DescriptorKind, JsObject, PropertyDescriptor};
use crate::string::{JsString, StringError, StringHeap};
use crate::{NativeCtx, Value};
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`NativeFunctionBody`].
pub const NATIVE_FUNCTION_BODY_TYPE_TAG: u8 = 0x1d;

/// Function-pointer signature for native callables.
///
/// `ctx` is the isolate-bound native view. Native bodies enqueue
/// work but **must not** synchronously re-enter the dispatch loop.
/// JS-side callbacks that need to run in turn (e.g. promise
/// reactions) flow through the microtask queue.
///
/// `args` is the JS argument list (post-coercion of any `apply`
/// expansion). Implementations return `Ok(value)` to write into
/// the call-site destination register, or `Err` to surface as a
/// runtime error.
pub type NativeFn = dyn for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
    + Send
    + Sync;

type LocalNativeFn =
    dyn for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>;

#[derive(Debug, Clone)]
enum NativeOwnProperty {
    Builtin,
    Deleted,
    Overridden(PropertyDescriptor),
}

#[derive(Debug, Clone, Copy)]
struct NativeFunctionMetadata {
    name_configurable: bool,
    length_configurable: bool,
    constructable: bool,
}

impl NativeFunctionMetadata {
    const BUILTIN: Self = Self {
        name_configurable: true,
        length_configurable: true,
        constructable: false,
    };

    const CONSTRUCTOR: Self = Self {
        name_configurable: true,
        length_configurable: true,
        constructable: true,
    };

    const THROW_TYPE_ERROR: Self = Self {
        name_configurable: false,
        length_configurable: false,
        constructable: false,
    };
}

/// Function-pointer signature for static builtin callables.
///
/// This is the production fast path for spec-declared builtins and
/// future macro-generated surfaces: invoking it requires no closure
/// allocation, capture clone, or dynamic dispatch.
pub type NativeFastFn = for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>;

/// VM-owned intrinsic callable.
///
/// These functions are JS-visible function values, but their
/// semantics require interpreter stack access rather than the
/// host-native [`NativeCtx`] boundary. The dispatch loop handles
/// them directly before the ordinary native-call path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmIntrinsicFunction {
    /// `Function.prototype.call`.
    FunctionPrototypeCall,
    /// `Function.prototype.apply`.
    FunctionPrototypeApply,
    /// `Function.prototype.bind`.
    FunctionPrototypeBind,
    /// `Function.prototype.toString`.
    FunctionPrototypeToString,
    /// `Function.prototype[@@hasInstance]` — §20.2.3.6.
    FunctionPrototypeSymbolHasInstance,
}

/// Native callable storage.
///
/// Static specs should use [`NativeCall::Static`]. Dynamic closures
/// are reserved for embedder cases that need captured Rust state.
#[derive(Clone)]
pub enum NativeCall {
    /// Plain function-pointer dispatch with no captured payload.
    Static(NativeFastFn),
    /// VM-owned intrinsic function dispatched by the interpreter.
    VmIntrinsic(VmIntrinsicFunction),
    /// Dynamic closure dispatch. Captured JS values still live in
    /// [`NativeFunctionBody::captures`] so the GC can trace them.
    Dynamic(Arc<NativeFn>),
}

#[derive(Clone)]
enum NativeCallStorage {
    Static(NativeFastFn),
    VmIntrinsic(VmIntrinsicFunction),
    Dynamic(Arc<NativeFn>),
    LocalDynamic(Rc<LocalNativeFn>),
}

impl From<NativeCall> for NativeCallStorage {
    fn from(value: NativeCall) -> Self {
        match value {
            NativeCall::Static(call) => Self::Static(call),
            NativeCall::VmIntrinsic(intrinsic) => Self::VmIntrinsic(intrinsic),
            NativeCall::Dynamic(call) => Self::Dynamic(call),
        }
    }
}

impl std::fmt::Debug for NativeCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(_) => f.write_str("NativeCall::Static(..)"),
            Self::VmIntrinsic(intrinsic) => f
                .debug_tuple("NativeCall::VmIntrinsic")
                .field(intrinsic)
                .finish(),
            Self::Dynamic(_) => f.write_str("NativeCall::Dynamic(..)"),
        }
    }
}

/// Optional tracing hook for native payloads whose Rust-side state
/// owns JS values outside the fixed capture list.
pub type NativeTraceFn = dyn Fn(&mut SlotVisitor<'_>);

/// Heap payload for [`Value::NativeFunction`].
pub struct NativeFunctionBody {
    /// Display name (used in stack traces and `Function.prototype.
    /// toString` once that lands).
    name: &'static str,
    /// ECMAScript `.length` metadata.
    length: u8,
    /// Static function pointer or dynamic closure payload.
    call: NativeCallStorage,
    /// JS values owned by the native payload and therefore traced
    /// strongly while this function is reachable.
    captures: SmallVec<[Value; 4]>,
    /// Optional trace hook for native-owned state such as shared
    /// Promise combinator slots.
    trace: Option<Rc<NativeTraceFn>>,
    /// Own property state for the built-in `name` property.
    name_property: NativeOwnProperty,
    /// Own property state for the built-in `length` property.
    length_property: NativeOwnProperty,
    /// Attribute policy for built-in metadata descriptors.
    metadata: NativeFunctionMetadata,
    /// Ordinary own properties installed on native callables, such
    /// as `%Proxy%.revocable`.
    own_properties: JsObject,
}

impl otter_gc::SafeTraceable for NativeFunctionBody {
    const TYPE_TAG: u8 = NATIVE_FUNCTION_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        for value in &self.captures {
            value.trace_value_slots(visitor);
        }
        if let Some(trace) = &self.trace {
            trace(visitor);
        }
        trace_native_own_property(&self.name_property, visitor);
        trace_native_own_property(&self.length_property, visitor);
        let p = &self.own_properties as *const JsObject as *mut RawGc;
        visitor(p);
    }
}

fn trace_native_own_property(property: &NativeOwnProperty, visitor: &mut SlotVisitor<'_>) {
    let NativeOwnProperty::Overridden(desc) = property else {
        return;
    };
    match &desc.kind {
        DescriptorKind::Data { value } => value.trace_value_slots(visitor),
        DescriptorKind::Accessor { getter, setter } => {
            if let Some(getter) = getter {
                getter.trace_value_slots(visitor);
            }
            if let Some(setter) = setter {
                setter.trace_value_slots(visitor);
            }
        }
    }
}

fn default_name_property() -> NativeOwnProperty {
    NativeOwnProperty::Builtin
}

fn default_length_property() -> NativeOwnProperty {
    NativeOwnProperty::Builtin
}

/// Cheap-to-clone native-function handle.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct NativeFunction {
    inner: otter_gc::Gc<NativeFunctionBody>,
}

impl std::fmt::Debug for NativeFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFunction")
            .field("inner", &self.inner)
            .finish()
    }
}

fn no_roots(_: &mut dyn FnMut(*mut RawGc)) {}

impl NativeFunction {
    fn allocate_with_roots(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeCallStorage,
        captures: SmallVec<[Value; 4]>,
        trace: Option<Rc<NativeTraceFn>>,
        metadata: NativeFunctionMetadata,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let own_properties = {
            let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                for value in &captures {
                    value.trace_value_slots(visitor);
                }
                if let Some(trace) = &trace {
                    trace(visitor);
                }
            };
            crate::object::alloc_object_with_roots(heap, &mut visit)?
        };
        let own_properties_root = Value::Object(own_properties);
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            own_properties_root.trace_value_slots(visitor);
            for value in &captures {
                value.trace_value_slots(visitor);
            }
            if let Some(trace) = &trace {
                trace(visitor);
            }
        };
        Ok(Self {
            inner: heap.alloc_with_roots(
                NativeFunctionBody {
                    name,
                    length,
                    call,
                    captures: captures.clone(),
                    trace: trace.clone(),
                    name_property: default_name_property(),
                    length_property: default_length_property(),
                    metadata,
                    own_properties,
                },
                &mut visit,
            )?,
        })
    }

    /// Build a native function with a static name and an `Fn`
    /// payload.
    pub fn new<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        call: F,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
            + Send
            + Sync
            + 'static,
    {
        Self::with_length_and_captures(heap, name, 0, call, SmallVec::new())
    }

    /// Build a static native function with explicit `.length`.
    pub fn new_static(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let mut external_visit = no_roots;
        Self::allocate_with_roots(
            heap,
            name,
            length,
            NativeCallStorage::Static(call),
            SmallVec::new(),
            None,
            NativeFunctionMetadata::BUILTIN,
            &mut external_visit,
        )
    }

    /// Build a static native function while exposing caller-owned
    /// roots across the metadata property-bag and body allocations.
    pub fn new_static_with_roots(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Self::allocate_with_roots(
            heap,
            name,
            length,
            NativeCallStorage::Static(call),
            SmallVec::new(),
            None,
            NativeFunctionMetadata::BUILTIN,
            external_visit,
        )
    }

    /// Build a static native function that has `[[Construct]]`.
    pub fn new_constructor_static(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let mut external_visit = no_roots;
        Self::allocate_with_roots(
            heap,
            name,
            length,
            NativeCallStorage::Static(call),
            SmallVec::new(),
            None,
            NativeFunctionMetadata::CONSTRUCTOR,
            &mut external_visit,
        )
    }

    /// Build a static native function that has `[[Construct]]`
    /// while exposing caller-owned roots across metadata allocation.
    pub(crate) fn new_constructor_static_with_roots(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Self::allocate_with_roots(
            heap,
            name,
            length,
            NativeCallStorage::Static(call),
            SmallVec::new(),
            None,
            NativeFunctionMetadata::CONSTRUCTOR,
            external_visit,
        )
    }

    /// Build a native function from an already-classified call
    /// target.
    pub fn from_call(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeCall,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let mut external_visit = no_roots;
        Self::allocate_with_roots(
            heap,
            name,
            length,
            call.into(),
            SmallVec::new(),
            None,
            NativeFunctionMetadata::BUILTIN,
            &mut external_visit,
        )
    }

    /// Build a native function from an already-classified call
    /// target while exposing caller-owned roots across the metadata
    /// property-bag and body allocations.
    pub fn from_call_with_roots(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeCall,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Self::allocate_with_roots(
            heap,
            name,
            length,
            call.into(),
            SmallVec::new(),
            None,
            NativeFunctionMetadata::BUILTIN,
            external_visit,
        )
    }

    /// Build the realm's `%ThrowTypeError%` intrinsic function while
    /// exposing caller-owned roots across metadata allocation.
    pub(crate) fn throw_type_error_with_roots(
        heap: &mut otter_gc::GcHeap,
        call: NativeFastFn,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        Self::allocate_with_roots(
            heap,
            "",
            0,
            NativeCallStorage::Static(call),
            SmallVec::new(),
            None,
            NativeFunctionMetadata::THROW_TYPE_ERROR,
            external_visit,
        )
    }

    /// Build a native function with explicit traced JS captures.
    pub fn with_captures<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        call: F,
        captures: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
            + Send
            + Sync
            + 'static,
    {
        Self::with_length_and_captures(heap, name, 0, call, captures)
    }

    /// Build a dynamic native function with explicit `.length` and
    /// explicit traced JS captures.
    pub fn with_length_and_captures<F>(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: F,
        captures: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory>
    where
        F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
            + Send
            + Sync
            + 'static,
    {
        let mut external_visit = no_roots;
        Self::allocate_with_roots(
            heap,
            name,
            length,
            NativeCallStorage::Dynamic(Arc::new(call)),
            captures,
            None,
            NativeFunctionMetadata::BUILTIN,
            &mut external_visit,
        )
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
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

    /// Read display metadata.
    #[must_use]
    pub fn name(&self, heap: &otter_gc::GcHeap) -> &'static str {
        heap.read_payload(self.inner, |body| body.name)
    }

    /// Read ECMAScript `.length` metadata.
    #[must_use]
    pub fn length(&self, heap: &otter_gc::GcHeap) -> u8 {
        heap.read_payload(self.inner, |body| body.length)
    }

    /// Whether this native function has `[[Construct]]`.
    #[must_use]
    pub(crate) fn is_constructable(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.metadata.constructable)
    }

    /// Return an own property descriptor for native function object
    /// metadata. Built-in `name` / `length` are non-writable,
    /// non-enumerable, configurable data properties.
    pub(crate) fn own_property_descriptor(
        &self,
        heap: &otter_gc::GcHeap,
        string_heap: &StringHeap,
        key: &str,
    ) -> Result<Option<PropertyDescriptor>, StringError> {
        heap.read_payload(self.inner, |body| {
            let property = match key {
                "name" => &body.name_property,
                "length" => &body.length_property,
                _ => {
                    return Ok(crate::object::get_own_descriptor(
                        body.own_properties,
                        heap,
                        key,
                    ));
                }
            };
            match property {
                NativeOwnProperty::Builtin => {
                    native_builtin_descriptor(body, string_heap, key).map(Some)
                }
                NativeOwnProperty::Deleted => Ok(None),
                NativeOwnProperty::Overridden(desc) => Ok(Some(desc.clone())),
            }
        })
    }

    /// Return an own symbol-keyed property descriptor stored on the
    /// native function object's ordinary property bag.
    pub(crate) fn own_symbol_property_descriptor(
        &self,
        heap: &otter_gc::GcHeap,
        key: &crate::symbol::JsSymbol,
    ) -> Option<PropertyDescriptor> {
        heap.read_payload(self.inner, |body| {
            crate::object::get_own_symbol_descriptor(body.own_properties, heap, key)
        })
    }

    /// Return enumerable own string keys for the function object's
    /// metadata properties. Built-in `name` / `length` are not
    /// enumerable; overridden descriptors participate according to
    /// their current `[[Enumerable]]` flag.
    #[must_use]
    pub(crate) fn enumerable_own_property_keys(&self, heap: &otter_gc::GcHeap) -> Vec<String> {
        heap.read_payload(self.inner, |body| {
            let mut keys = Vec::new();
            if native_own_property_is_enumerable(&body.name_property, false) {
                keys.push("name".to_string());
            }
            if native_own_property_is_enumerable(&body.length_property, false) {
                keys.push("length".to_string());
            }
            keys.extend(crate::object::with_properties(
                body.own_properties,
                heap,
                |p| p.enumerable_keys().map(str::to_string).collect::<Vec<_>>(),
            ));
            keys
        })
    }

    /// Return own string property keys in built-in function
    /// creation order: `length`, then `name`.
    #[must_use]
    pub(crate) fn own_property_keys(&self, heap: &otter_gc::GcHeap) -> Vec<String> {
        heap.read_payload(self.inner, |body| {
            let mut keys = Vec::new();
            if !matches!(body.length_property, NativeOwnProperty::Deleted) {
                keys.push("length".to_string());
            }
            if !matches!(body.name_property, NativeOwnProperty::Deleted) {
                keys.push("name".to_string());
            }
            keys.extend(crate::object::with_properties(
                body.own_properties,
                heap,
                |p| p.keys().map(str::to_string).collect::<Vec<_>>(),
            ));
            keys
        })
    }

    /// Define or redefine one of the native function object's own
    /// metadata properties. This slice supports `name` / `length`;
    /// arbitrary expando properties still belong to the broader
    /// function-object property-bag work.
    pub(crate) fn define_own_property(
        &self,
        heap: &mut otter_gc::GcHeap,
        string_heap: &StringHeap,
        key: &str,
        descriptor: PropertyDescriptor,
    ) -> bool {
        let existing = match self.own_property_descriptor(heap, string_heap, key) {
            Ok(existing) => existing,
            Err(_) => return false,
        };
        let descriptor = match existing {
            Some(existing) => {
                match crate::object::validate_descriptor_update(&existing, &descriptor) {
                    Some(merged) => merged,
                    None => return false,
                }
            }
            None if key == "name" || key == "length" => descriptor,
            None => {
                let obj = heap.read_payload(self.inner, |body| body.own_properties);
                return crate::object::define_own_property(obj, heap, key, descriptor);
            }
        };
        // Built-in `name` / `length` slots live on the metadata
        // record so future spec reads see the override without
        // walking the side-table. Every other key — including
        // existing builder-installed methods like
        // `Promise.resolve` — routes through `body.own_properties`
        // so accessor / data redefinitions on a NativeFunction
        // ctor work uniformly.
        if key != "name" && key != "length" {
            let obj = heap.read_payload(self.inner, |body| body.own_properties);
            return crate::object::define_own_property(obj, heap, key, descriptor);
        }
        let barrier_descriptor = descriptor.clone();
        let success = heap.with_payload(self.inner, |body| {
            let slot = match key {
                "name" => &mut body.name_property,
                "length" => &mut body.length_property,
                _ => unreachable!(),
            };
            *slot = NativeOwnProperty::Overridden(descriptor);
            true
        });
        if success {
            heap.record_write(self.inner, &barrier_descriptor);
        }
        success
    }

    /// Define or redefine a symbol-keyed own property on the native
    /// function object's ordinary property bag.
    pub(crate) fn define_own_symbol_property(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &crate::symbol::JsSymbol,
        descriptor: crate::object::PartialPropertyDescriptor,
    ) -> bool {
        let obj = heap.read_payload(self.inner, |body| body.own_properties);
        crate::object::define_own_symbol_property_partial(obj, heap, key, descriptor)
    }

    /// Delete a configurable own metadata property.
    pub(crate) fn delete_own_property(&self, heap: &mut otter_gc::GcHeap, key: &str) -> bool {
        if key != "name" && key != "length" {
            let own_properties = heap.read_payload(self.inner, |body| body.own_properties);
            return crate::object::delete(own_properties, heap, key);
        }
        heap.with_payload(self.inner, |body| {
            let slot = match key {
                "name" => &mut body.name_property,
                "length" => &mut body.length_property,
                _ => return true,
            };
            let configurable = match slot {
                NativeOwnProperty::Builtin => match key {
                    "name" => body.metadata.name_configurable,
                    "length" => body.metadata.length_configurable,
                    _ => true,
                },
                NativeOwnProperty::Deleted => return true,
                NativeOwnProperty::Overridden(desc) => desc.configurable(),
            };
            if !configurable {
                return false;
            }
            *slot = NativeOwnProperty::Deleted;
            true
        })
    }

    /// Delete a configurable symbol-keyed own property from the
    /// native function object's ordinary property bag.
    pub(crate) fn delete_own_symbol_property(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &crate::symbol::JsSymbol,
    ) -> bool {
        let own_properties = heap.read_payload(self.inner, |body| body.own_properties);
        crate::object::delete_symbol(own_properties, heap, key)
    }

    /// Clone the call target and captures so the caller can invoke
    /// it after releasing the heap borrow.
    #[must_use]
    pub(crate) fn call_target(&self, heap: &otter_gc::GcHeap) -> NativeCallTarget {
        heap.read_payload(self.inner, |body| match &body.call {
            NativeCallStorage::Static(call) => NativeCallTarget::Static(*call),
            NativeCallStorage::VmIntrinsic(intrinsic) => NativeCallTarget::VmIntrinsic(*intrinsic),
            NativeCallStorage::Dynamic(call) => NativeCallTarget::Dynamic {
                call: call.clone(),
                captures: body.captures.clone(),
            },
            NativeCallStorage::LocalDynamic(call) => NativeCallTarget::LocalDynamic {
                call: call.clone(),
                captures: body.captures.clone(),
            },
        })
    }

    /// `true` when this callable uses the static function-pointer
    /// fast path.
    #[must_use]
    pub fn is_static_call(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| {
            matches!(body.call, NativeCallStorage::Static(_))
        })
    }

    /// `true` when this callable resolves to the named VM intrinsic.
    /// Used by §13.10.2 InstanceofOperator's fast path so the spec's
    /// `Call(Function.prototype[@@hasInstance], target, « V »)`
    /// dispatches straight to OrdinaryHasInstance instead of pushing
    /// an extra frame.
    #[must_use]
    pub fn is_vm_intrinsic(&self, heap: &otter_gc::GcHeap, intrinsic: VmIntrinsicFunction) -> bool {
        heap.read_payload(
            self.inner,
            |body| matches!(body.call, NativeCallStorage::VmIntrinsic(i) if i == intrinsic),
        )
    }

    /// Trace this handle as a root slot.
    pub(crate) fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = self as *const NativeFunction as *mut RawGc;
        visitor(p);
    }
}

/// Cloned native target ready for invocation after the heap borrow
/// has ended.
pub(crate) enum NativeCallTarget {
    /// Static fast path.
    Static(NativeFastFn),
    /// VM-owned intrinsic function.
    VmIntrinsic(VmIntrinsicFunction),
    /// Dynamic closure path with traced captures.
    Dynamic {
        /// Closure payload.
        call: Arc<NativeFn>,
        /// Traced JS captures.
        captures: SmallVec<[Value; 4]>,
    },
    /// Local VM-only closure path.
    LocalDynamic {
        /// Closure payload.
        call: Rc<LocalNativeFn>,
        /// Traced JS captures.
        captures: SmallVec<[Value; 4]>,
    },
}

impl NativeCallTarget {
    /// Invoke the target.
    pub(crate) fn invoke(
        self,
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        match self {
            Self::Static(call) => call(ctx, args),
            Self::VmIntrinsic(intrinsic) => Err(NativeError::TypeError {
                name: intrinsic.name(),
                reason: "VM intrinsic requires interpreter dispatch".to_string(),
            }),
            Self::Dynamic { call, captures } => call(ctx, args, &captures),
            Self::LocalDynamic { call, captures } => call(ctx, args, &captures),
        }
    }
}

impl VmIntrinsicFunction {
    /// JS-visible builtin function name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FunctionPrototypeCall => "call",
            Self::FunctionPrototypeApply => "apply",
            Self::FunctionPrototypeBind => "bind",
            Self::FunctionPrototypeToString => "toString",
            Self::FunctionPrototypeSymbolHasInstance => "[Symbol.hasInstance]",
        }
    }
}

/// Convenience: produce a `Value::NativeFunction` from a closure.
pub fn native_value<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
        + Send
        + Sync
        + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::new(
        heap, name, call,
    )?))
}

/// Convenience: produce a static native function value.
pub fn native_value_static(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: NativeFastFn,
) -> Result<Value, otter_gc::OutOfMemory> {
    Ok(Value::NativeFunction(NativeFunction::new_static(
        heap, name, length, call,
    )?))
}

/// Convenience: produce a native function with explicit traced JS
/// captures.
pub fn native_value_with_captures<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
        + Send
        + Sync
        + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::with_captures(
        heap, name, call, captures,
    )?))
}

pub(crate) fn native_constructor_value_with_captures_unchecked_with_roots<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    external_visit: &mut RootSlotVisitor<'_>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::allocate_with_roots(
        heap,
        name,
        0,
        NativeCallStorage::LocalDynamic(Rc::new(call)),
        captures,
        None,
        NativeFunctionMetadata::CONSTRUCTOR,
        external_visit,
    )?))
}

pub(crate) fn native_value_with_captures_unchecked_with_roots<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    external_visit: &mut RootSlotVisitor<'_>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::allocate_with_roots(
        heap,
        name,
        0,
        NativeCallStorage::LocalDynamic(Rc::new(call)),
        captures,
        None,
        NativeFunctionMetadata::BUILTIN,
        external_visit,
    )?))
}

pub(crate) fn native_value_with_trace_unchecked_with_roots<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    captures: SmallVec<[Value; 4]>,
    trace: Rc<NativeTraceFn>,
    external_visit: &mut RootSlotVisitor<'_>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::NativeFunction(NativeFunction::allocate_with_roots(
        heap,
        name,
        0,
        NativeCallStorage::LocalDynamic(Rc::new(call)),
        captures,
        Some(trace),
        NativeFunctionMetadata::BUILTIN,
        external_visit,
    )?))
}

fn native_builtin_descriptor(
    body: &NativeFunctionBody,
    string_heap: &StringHeap,
    key: &str,
) -> Result<PropertyDescriptor, StringError> {
    let value = match key {
        "name" => Value::String(JsString::from_str(body.name, string_heap)?),
        "length" => Value::Number(crate::number::NumberValue::from_i32(body.length as i32)),
        _ => Value::Undefined,
    };
    let configurable = match key {
        "name" => body.metadata.name_configurable,
        "length" => body.metadata.length_configurable,
        _ => true,
    };
    Ok(PropertyDescriptor::data(value, false, false, configurable))
}

fn native_own_property_is_enumerable(property: &NativeOwnProperty, builtin_default: bool) -> bool {
    match property {
        NativeOwnProperty::Builtin => builtin_default,
        NativeOwnProperty::Deleted => false,
        NativeOwnProperty::Overridden(desc) => desc.flags.enumerable(),
    }
}

/// Failure outcome from a native call. Mirrors the
/// [`crate::IntrinsicError`] / [`crate::math::MathError`] shape so
/// the runtime mapper can route everything through one path.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum NativeError {
    /// A user-thrown JS value escaped the native body. The
    /// dispatcher will route this through the same path as
    /// `Op::Throw` — i.e. into the catchable handler stack.
    #[error("native function {name} threw")]
    Thrown {
        /// Display name of the offending native (for diagnostics).
        name: &'static str,
        /// The thrown value. Foundation: rendered to a string.
        message: String,
    },
    /// Type or value error inside the native body that does not
    /// originate as a `throw` (e.g. wrong arity). Surfaces as
    /// `VmError::TypeMismatch`.
    #[error("native function {name}: {reason}")]
    TypeError {
        /// Display name of the native.
        name: &'static str,
        /// Short reason.
        reason: String,
    },
    /// Syntax error reported by a native that performs dynamic
    /// source compilation, such as the `Function` constructor.
    #[error("native function {name}: {reason}")]
    SyntaxError {
        /// Display name of the native.
        name: &'static str,
        /// Short reason.
        reason: String,
    },
    /// Out-of-range argument; surfaces as a JS `RangeError`. Used
    /// by intrinsics whose spec wording mandates `RangeError`
    /// (e.g. `Number.prototype.toFixed`, `toExponential`,
    /// `toPrecision` — out-of-range `fractionDigits` / `precision`).
    #[error("native function {name}: {reason}")]
    RangeError {
        /// Display name of the native.
        name: &'static str,
        /// Short reason.
        reason: String,
    },
    /// Host-visible runtime termination requested by a native such
    /// as `process.exit(code)`. This is not a JS throw and must not
    /// be catchable by user code.
    #[error("native function requested process exit with code {code}")]
    Exit {
        /// Process-style exit status, already normalized to one byte.
        code: u8,
    },
}

impl From<otter_gc::OutOfMemory> for NativeError {
    fn from(_: otter_gc::OutOfMemory) -> Self {
        Self::TypeError {
            name: "native",
            reason: "out of memory".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::number::NumberValue;

    #[test]
    fn native_value_dispatches() {
        let mut interp = crate::Interpreter::new();
        let f = native_value(interp.gc_heap_mut(), "identity", |_, args, _captures| {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        })
        .expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!("expected NativeFunction")
        };
        let call = native.call_target(interp.gc_heap());
        let mut ctx = NativeCtx::new(&mut interp);
        let r = call
            .invoke(&mut ctx, &[Value::Number(NumberValue::from_i32(7))])
            .unwrap();
        assert_eq!(r.display_string(), "7");
    }

    #[test]
    fn rejects_arity_via_typeerror() {
        let mut interp = crate::Interpreter::new();
        let f = native_value(
            interp.gc_heap_mut(),
            "require_one_arg",
            |_, args, _captures| {
                if args.len() != 1 {
                    return Err(NativeError::TypeError {
                        name: "require_one_arg",
                        reason: format!("expected 1 arg, got {}", args.len()),
                    });
                }
                Ok(args[0].clone())
            },
        )
        .expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!()
        };
        let call = native.call_target(interp.gc_heap());
        let mut ctx = NativeCtx::new(&mut interp);
        let err = call.invoke(&mut ctx, &[]).unwrap_err();
        assert!(matches!(err, NativeError::TypeError { .. }));
    }

    #[test]
    fn static_native_value_uses_fast_path_and_length() {
        fn id(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.first().cloned().unwrap_or(Value::Undefined))
        }

        let mut interp = crate::Interpreter::new();
        let f = native_value_static(interp.gc_heap_mut(), "id", 1, id).expect("native");
        let Value::NativeFunction(native) = &f else {
            panic!("expected NativeFunction")
        };
        assert!(native.is_static_call(interp.gc_heap()));
        assert_eq!(native.length(interp.gc_heap()), 1);
    }
}
