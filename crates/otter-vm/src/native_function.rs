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
//! - [`NativeFunctionBody`] — C call header, name and traced captures.
//! - [`NativeEntryKind`] — the immutable machine-visible payload kind.
//! - [`NativeFastFn`] / [`NativeCall`] — static and dynamic native
//!   dispatch targets.
//! - [`NativeFn`] — the dynamic closure signature.
//! - [`NativeError`] — authored failures and owned completed execution failures
//!   projected by the published Host owner.
//!
//! # Invariants
//! - Every allocation receives an explicit [`otter_gc::GcHeap`]; active
//!   VM-owned dynamic closures use the root-aware helper when caller roots are
//!   available.
//! - The call signature receives an explicit [`crate::NativeCtx`].
//!   Host async work must copy owned, non-GC data out before any
//!   `.await`; `NativeCtx`, `Value`, and GC handles are
//!   isolate-local.
//! - Every body starts with the one C call header. Static entries retain a
//!   typed function pointer; dynamic entries retain a host-table index. All
//!   policy mutation updates this header, and captures stay in a traced slab.
//! - Public dynamic native constructors require `Send + Sync`
//!   closures and pass traced JS captures as an explicit slice at
//!   call time. That keeps embedders from hiding isolate-local
//!   `Gc<T>` / `Value` handles inside a long-lived closure.
//! - Crate-internal unchecked constructors are reserved for audited
//!   isolate-local VM helpers. Their closures keep JS values in the
//!   capture slab and nowhere else: shared Rust state behind the
//!   closure's `Arc` must hold no `Value`, because nothing traces it.
//!
//! # See also
//! - [GC API](../../../docs/book/src/engine/gc-api.md)
//! - [Native bindings](../../../docs/book/src/extensions/native-bindings.md)

mod call_header;

use call_header::NativeCallHeader;
pub use call_header::NativeEntryKind;

use std::sync::Arc;

use smallvec::SmallVec;

use crate::object::{JsObject, PartialPropertyDescriptor, PropertyDescriptor};
use crate::string::JsString;
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

pub(crate) type LocalNativeFn =
    dyn for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>;

#[derive(Debug, Clone)]
enum NativeOwnProperty {
    Builtin,
    Deleted,
    Overridden(PropertyDescriptor),
}

impl crate::pelt::PeltField for NativeOwnProperty {
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        if let Self::Overridden(desc) = self {
            <PropertyDescriptor as crate::pelt::PeltField>::pelt_trace(desc, visitor);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct NativeFunctionMetadata {
    name_configurable: bool,
    length_configurable: bool,
    constructable: bool,
    extensible: bool,
}

impl NativeFunctionMetadata {
    const BUILTIN: Self = Self {
        name_configurable: true,
        length_configurable: true,
        constructable: false,
        extensible: true,
    };

    const CONSTRUCTOR: Self = Self {
        name_configurable: true,
        length_configurable: true,
        constructable: true,
        extensible: true,
    };

    const THROW_TYPE_ERROR: Self = Self {
        name_configurable: false,
        length_configurable: false,
        constructable: false,
        extensible: false,
    };
}

/// Function-pointer signature for static builtin callables.
///
/// Production fast path for spec-declared builtins and macro-generated
/// surfaces: no closure allocation, no capture clone, no dynamic
/// dispatch.
pub type NativeFastFn = for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>;

/// Plain function pointer for a static native that reads traced captures.
/// Same process-local snapshot identity as [`NativeFastFn`] — the entry has a
/// dense external-reference id — with the capture-slab slice the dynamic ABI
/// passes.
pub type NativeCapturesFn =
    for<'rt> fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>;

/// VM-owned intrinsic callable.
///
/// These functions are JS-visible function values, but their
/// semantics require interpreter stack access rather than the
/// host-native [`NativeCtx`] boundary. The dispatch loop handles
/// them directly before the ordinary native-call path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
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

impl VmIntrinsicFunction {
    /// Address naming this intrinsic in the isolate's external-reference
    /// table. An intrinsic has no entry point of its own, so each one owns a
    /// distinct byte of an immutable table instead: generated identity guards
    /// then compare its `native_ref` exactly as they compare a static
    /// builtin's.
    fn identity_address(self) -> usize {
        // One byte per variant; the last variant bounds the table.
        const COUNT: usize = VmIntrinsicFunction::FunctionPrototypeSymbolHasInstance as usize + 1;
        static IDENTITIES: [u8; COUNT] = [0; COUNT];
        std::ptr::from_ref(&IDENTITIES[self as usize]) as usize
    }

    /// External-reference index of this intrinsic in `heap`, or `None` when
    /// the isolate never installed it.
    #[must_use]
    pub(crate) fn native_ref(self, heap: &otter_gc::GcHeap) -> Option<u32> {
        heap.external_refs().lookup(self.identity_address())
    }
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
    StaticWithCaptures(NativeCapturesFn),
    VmIntrinsic(VmIntrinsicFunction),
    Dynamic(Arc<NativeFn>),
    LocalDynamic(Arc<LocalNativeFn>),
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

/// Heap payload for [`Value::NativeFunction`].
#[derive(otter_macros::Pelt)]
#[pelt(tag = NATIVE_FUNCTION_BODY_TYPE_TAG)]
#[repr(C)]
pub struct NativeFunctionBody {
    /// Authoritative machine-readable call target and callable policy.
    /// Its payload has no GC values; captures live immediately after it.
    #[pelt(skip)]
    call_header: NativeCallHeader,
    /// JS values owned by the native payload and therefore traced
    /// strongly while this function is reachable, in a
    /// [`crate::value_slab::ValueSlabBody`] of their own — null when
    /// the callable captures nothing, which is every static builtin.
    /// This is the ONLY place a dynamic closure may keep JS values:
    /// shared Rust state behind the closure's `Arc` must hold no
    /// `Value` (a counter is fine), because nothing traces it.
    captures: crate::value_slab::ValueSlabHandle,
    /// Display name retained and traced by this body.
    name: JsString,
    /// Own property state for the built-in `name` property.
    name_property: NativeOwnProperty,
    /// Own property state for the built-in `length` property.
    length_property: NativeOwnProperty,
    /// Ordinary own properties installed on native callables, such
    /// as `%Proxy%.revocable`.
    own_properties: JsObject,
    /// Override for `[[Prototype]]`. Defaults to `None` so
    /// `Object.getPrototypeOf` falls back to `%Function.prototype%`.
    /// Spec-mandated overrides (e.g. each concrete TypedArray ctor
    /// must inherit from `%TypedArray%`, §23.2.6) populate this slot
    /// at bootstrap. Stored as a [`Value`] so the override can itself
    /// be a callable (a NativeFunction such as `%TypedArray%`).
    prototype_override: Option<Value>,
    /// Realm global associated with this native function. `None`
    /// means the default active interpreter realm.
    realm_global: Option<JsObject>,
    /// Native callable identity is this GC body, never its shared entry pointer.
    constructor_layouts: crate::constructor_layout::ConstructorLayout,
}

/// One body layout for VM invocation, generated guards and callee dispatch.
pub(crate) fn jit_call_layout() -> crate::jit::JitNativeCallLayout {
    let header = (otter_gc::header::HEADER_SIZE
        + std::mem::offset_of!(NativeFunctionBody, call_header)) as u32;
    crate::jit::JitNativeCallLayout {
        identity_byte: header + call_header::NATIVE_REF_OFFSET as u32,
        kind_byte: header + call_header::KIND_OFFSET as u32,
        flags_byte: header + call_header::FLAGS_OFFSET as u32,
        payload_byte: header + call_header::PAYLOAD_OFFSET as u32,
        captures_byte: (otter_gc::header::HEADER_SIZE
            + std::mem::offset_of!(NativeFunctionBody, captures)) as u32,
        constructable_flag: NativeCallHeader::CONSTRUCTABLE,
        extensible_flag: NativeCallHeader::EXTENSIBLE,
    }
}

const _: () = {
    assert!(std::mem::offset_of!(NativeFunctionBody, call_header) == 0);
    assert!(std::mem::offset_of!(NativeFunctionBody, captures) == 16);
};

impl NativeFunctionBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for property in [&self.name_property, &self.length_property] {
            if let NativeOwnProperty::Overridden(descriptor) = property {
                crate::code_liveness::visit_descriptor(descriptor, visitor);
            }
        }
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }

    /// Describe the body's call header and traced capture extent for the
    /// native census, without exposing payload ownership to the aggregator.
    pub(crate) fn census_facts(&self) -> crate::native_census::NativeBodyFacts {
        use crate::native_census::{NativeBodyFacts, NativeStorageKind};
        let kind = match self.call_header.kind {
            NativeEntryKind::Static | NativeEntryKind::StaticWithCaptures => {
                NativeStorageKind::Static
            }
            NativeEntryKind::VmIntrinsic => NativeStorageKind::VmIntrinsic,
            NativeEntryKind::Dynamic => NativeStorageKind::Dynamic,
            NativeEntryKind::LocalDynamic => NativeStorageKind::LocalDynamic,
        };
        let static_addr = self.call_header.static_address();
        NativeBodyFacts {
            name: self.name,
            kind,
            static_addr,
            native_ref: self.call_header.native_ref,
            // SAFETY: `self` is a live payload borrow, which keeps the
            // slab it names reachable.
            capture_count: unsafe { crate::value_slab::live_slice(self.captures).len() },
        }
    }
}

/// Clone every dynamic-native closure entry of `source`'s host-ref
/// table into `target`, at identical indices.
///
/// The restored bodies carry the capture
/// isolate's `u32` indices, and the two payload types this table holds
/// are both `Arc`s that clone by reference count.
#[cfg(test)]
pub(crate) fn clone_host_refs_for_restore(
    source: &otter_gc::GcHeap,
    target: &mut otter_gc::GcHeap,
) {
    for (index, payload) in snapshot_dynamic_natives(source) {
        install_dynamic_native(target, index, &payload);
    }
}

/// Collect the dynamic-native closures of `heap`'s host-ref table for
/// the in-process snapshot, Arc-cloned at their indices.
pub(crate) fn snapshot_dynamic_natives(
    heap: &otter_gc::GcHeap,
) -> Vec<(u32, crate::snapshot::DynamicNativePayload)> {
    use crate::snapshot::DynamicNativePayload;
    heap.host_refs()
        .entries()
        .map(|(index, payload)| {
            let clone = if let Some(shared) = payload.downcast_ref::<Arc<NativeFn>>() {
                DynamicNativePayload::Shared(shared.clone())
            } else if let Some(local) = payload.downcast_ref::<Arc<LocalNativeFn>>() {
                DynamicNativePayload::Local(local.clone())
            } else {
                unreachable!("host-ref table holds only dynamic-native closures")
            };
            (index, clone)
        })
        .collect()
}

/// Install one snapshot-carried closure at its captured index.
pub(crate) fn install_dynamic_native(
    heap: &mut otter_gc::GcHeap,
    index: u32,
    payload: &crate::snapshot::DynamicNativePayload,
) {
    use crate::snapshot::DynamicNativePayload;
    let boxed: Box<dyn std::any::Any> = match payload {
        DynamicNativePayload::Shared(shared) => Box::new(shared.clone()),
        DynamicNativePayload::Local(local) => Box::new(local.clone()),
    };
    heap.host_refs_mut().insert_at(index, boxed);
}

impl otter_gc::trace::ReleaseHostRefs for NativeFunctionBody {
    fn release_host_refs(&mut self, table: &mut otter_gc::host_refs::HostRefTable) {
        if let Some(index) = self.call_header.host_ref() {
            table.release(index);
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
    /// Traced family head of this exact constructor object.
    pub(crate) fn constructor_layouts(
        self,
        heap: &otter_gc::GcHeap,
    ) -> crate::constructor_layout::ConstructorLayout {
        heap.read_payload(self.inner, |body| body.constructor_layouts)
    }
    pub(crate) fn set_constructor_layouts(
        self,
        heap: &mut otter_gc::GcHeap,
        layouts: crate::constructor_layout::ConstructorLayout,
    ) {
        heap.with_payload(self.inner, |body| body.constructor_layouts = layouts);
        heap.record_write(self.inner, &layouts);
    }

    fn allocate_with_roots(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeCallStorage,
        captures: SmallVec<[Value; 4]>,
        metadata: NativeFunctionMetadata,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        // The display name is allocated before the capture slab exists. Trace
        // the exact mutable vector that will later move into that slab during
        // this first allocation; tracing a clone would rewrite only the clone
        // and leave the published vector holding stale young-generation
        // handles after a moving collection.
        let mut captures = captures;
        let name_string = {
            let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                for value in &captures {
                    value.trace_value_slots(visitor);
                }
            };
            JsString::from_str_with_roots(name, heap, &mut visit)?
        };
        let mut name_root = Value::string(name_string);
        // Call payload ownership and external identity are published together.
        // Dynamic entries move their Arc to the host table; the body owns its
        // index and releases it when collected.
        let call_header = NativeCallHeader::allocate(heap, call, metadata, length);
        let mut own_properties = {
            let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                name_root.trace_value_slots(visitor);
                for value in &captures {
                    value.trace_value_slots(visitor);
                }
            };
            crate::object::alloc_dictionary_object_with_roots(heap, &mut visit)?
        };
        if !metadata.extensible {
            use crate::rooting::RootScopeExt;
            let mut pending = otter_gc::RootScope::new(heap);
            // SAFETY: captures/name are actual stationary payload slots; this
            // scope closes before captures moves into its slab below.
            unsafe {
                pending.add_value(&mut name_root);
                pending.add_value_smallvec(&mut captures);
            }
            crate::object::prevent_extensions(&mut own_properties, heap)?;
        }
        let own_properties_root = Value::object(own_properties);
        // The captures move into a slab body of their own; the shell
        // below names it by handle. `slab_from_values` roots the pending
        // values itself and remembers the copied-in edges.
        let mut captures_slab = {
            let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external_visit(visitor);
                name_root.trace_value_slots(visitor);
                own_properties_root.trace_value_slots(visitor);
            };
            crate::value_slab::slab_from_values(heap, &mut captures, &mut visit)?
        };
        let captures_slot = std::ptr::addr_of_mut!(captures_slab);
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            name_root.trace_value_slots(visitor);
            own_properties_root.trace_value_slots(visitor);
            visitor(captures_slot.cast::<RawGc>());
        };
        // The object and capture-slab allocations above may have moved both
        // roots. Rebuild the typed wrappers from the rewritten root slots
        // before constructing the pending body. Reusing the original locals
        // here would publish stale from-space handles whenever one of those
        // intermediate allocations collected without the final body
        // allocation collecting again.
        let name_string = name_root
            .as_string(heap)
            .expect("native display-name root remains a string");
        let own_properties = own_properties_root
            .as_object()
            .expect("native own-properties root remains an object");
        Ok(Self {
            inner: heap.alloc_with_roots(
                NativeFunctionBody {
                    call_header,
                    name: name_string,
                    captures: captures_slab,
                    name_property: default_name_property(),
                    length_property: default_length_property(),
                    own_properties,
                    prototype_override: None,
                    realm_global: None,
                    constructor_layouts: crate::constructor_layout::ConstructorLayout::null(),
                },
                &mut visit,
            )?,
        })
    }

    /// Spec-driven `[[Prototype]]` override. Concrete TypedArray
    /// constructors point at `%TypedArray%` per §23.2.6.1.
    /// <https://tc39.es/ecma262/#sec-properties-of-the-typedarray-constructors>
    pub fn set_prototype_override(&self, heap: &mut otter_gc::GcHeap, proto: Option<Value>) {
        let proto_clone = proto;
        let success = heap.with_payload(self.inner, |body| {
            body.prototype_override = proto;
            true
        });
        if success && let Some(p) = proto_clone {
            heap.record_write(self.inner, &p);
        }
    }

    /// Set the native function's associated realm global.
    pub fn set_realm_global(&self, heap: &mut otter_gc::GcHeap, global: Option<JsObject>) {
        let global_clone = global;
        let success = heap.with_payload(self.inner, |body| {
            body.realm_global = global;
            true
        });
        if success && let Some(global) = global_clone {
            heap.record_write(self.inner, &global);
        }
    }

    /// Associated realm global, if this native belongs to a non-default realm.
    #[must_use]
    pub fn realm_global(&self, heap: &otter_gc::GcHeap) -> Option<JsObject> {
        heap.read_payload(self.inner, |body| body.realm_global)
    }

    /// The own-property metadata bag (name/length plus user-defined own
    /// properties). Realm bootstrap walks it to stamp nested callables.
    #[must_use]
    pub(crate) fn own_properties_bag(&self, heap: &otter_gc::GcHeap) -> JsObject {
        heap.read_payload(self.inner, |body| body.own_properties)
    }

    /// Current `[[Prototype]]` override, if set.
    #[must_use]
    pub fn prototype_override(&self, heap: &otter_gc::GcHeap) -> Option<Value> {
        heap.read_payload(self.inner, |body| body.prototype_override)
    }

    /// `true` when this native function's `[[Call]]` is exactly the static
    /// builtin `target`. Lets a hot caller recognise an un-monkey-patched
    /// intrinsic (e.g. the original `RegExp.prototype.exec`) and bypass the
    /// observable protocol; any override (a different function, or a
    /// dynamic/closure callable) returns `false`.
    #[must_use]
    pub(crate) fn is_static_native(&self, heap: &otter_gc::GcHeap, target: NativeFastFn) -> bool {
        self.static_fn(heap)
            .is_some_and(|call| std::ptr::fn_addr_eq(call, target))
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
        Self::with_length_and_closure(heap, name, 0, call, SmallVec::new())
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
            NativeFunctionMetadata::BUILTIN,
            external_visit,
        )
    }

    /// Build a native constructor from an already-classified call target while
    /// exposing caller-owned roots across metadata allocation.
    pub fn from_constructor_call_with_roots(
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
            NativeFunctionMetadata::CONSTRUCTOR,
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
        Self::with_length_and_closure(heap, name, 0, call, captures)
    }

    /// Build a static native function with explicit `.length` and
    /// explicit traced JS captures. The entry point is a plain `fn`, so the
    /// callable needs no closure allocation: its identity rides the
    /// external-reference table and the captures ride the slab.
    pub fn with_length_and_captures(
        heap: &mut otter_gc::GcHeap,
        name: &'static str,
        length: u8,
        call: NativeCapturesFn,
        captures: SmallVec<[Value; 4]>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let mut external_visit = no_roots;
        Self::allocate_with_roots(
            heap,
            name,
            length,
            NativeCallStorage::StaticWithCaptures(call),
            captures,
            NativeFunctionMetadata::BUILTIN,
            &mut external_visit,
        )
    }

    /// Build a genuinely dynamic native function from a Rust closure,
    /// with explicit `.length` and traced JS captures. Prefer the
    /// `fn`-pointer constructors when captured Rust state is unnecessary. A
    /// closure's `Arc` lives in the host-ref table and is Arc-cloned at its
    /// exact index by an in-process snapshot.
    fn with_length_and_closure<F>(
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
            NativeFunctionMetadata::BUILTIN,
            &mut external_visit,
        )
    }

    /// Raw handle used by root tracing and write barriers.
    #[must_use]
    pub(crate) fn raw(&self) -> RawGc {
        self.inner.raw()
    }

    /// Reinterpret a body handle as the public [`NativeFunction`]
    /// wrapper. Used by [`crate::value::Value::as_native_function`]
    /// after a `GcHeader::type_tag` check has confirmed the body is
    /// a [`NativeFunctionBody`].
    #[inline]
    #[must_use]
    pub fn from_gc(inner: otter_gc::Gc<NativeFunctionBody>) -> Self {
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

    /// Read display metadata.
    #[must_use]
    pub fn name(&self, heap: &otter_gc::GcHeap) -> JsString {
        heap.read_payload(self.inner, |body| body.name)
    }

    /// Display name as a Rust `String`, for diagnostics and error
    /// message formatting.
    #[must_use]
    pub fn name_string(&self, heap: &otter_gc::GcHeap) -> String {
        self.name(heap).to_lossy_string(heap)
    }

    /// Whether the display name equals `expected`, without allocating.
    /// Hot dispatch paths use this to recognise specific builtins.
    #[must_use]
    pub(crate) fn name_is(&self, heap: &otter_gc::GcHeap, expected: &str) -> bool {
        self.name(heap).eq_str(expected, heap)
    }

    /// Read ECMAScript `.length` metadata.
    #[must_use]
    pub fn length(&self, heap: &otter_gc::GcHeap) -> u8 {
        heap.read_payload(self.inner, |body| body.call_header.length)
    }

    /// Whether this native function has `[[Construct]]`.
    #[must_use]
    pub(crate) fn is_constructable(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.call_header.constructable())
    }

    /// Native callable `[[IsExtensible]]`.
    #[must_use]
    pub(crate) fn is_extensible(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| body.call_header.extensible())
    }

    /// Native callable `[[PreventExtensions]]`.
    pub(crate) fn prevent_extensions(
        &self,
        heap: &mut otter_gc::GcHeap,
    ) -> Result<(), otter_gc::OutOfMemory> {
        let mut owner = self.inner;
        let mut roots = otter_gc::RootScope::new(heap);
        // SAFETY: the native shell is old-space; this stationary root keeps it
        // live through preparation, and no body borrow crosses the allocation.
        unsafe {
            roots.add_raw_slot(std::ptr::addr_of_mut!(owner).cast::<RawGc>());
        }
        let mut own_properties = heap.read_payload(owner, |body| body.own_properties);
        crate::object::prevent_extensions(&mut own_properties, heap)?;
        heap.with_payload(owner, |body| body.call_header.prevent_extensions());
        Ok(())
    }

    /// `Object.isSealed` for native callable values.
    #[must_use]
    pub(crate) fn is_sealed(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| {
            !body.call_header.extensible()
                && native_own_property_is_sealed(
                    &body.name_property,
                    body.call_header.name_configurable(),
                )
                && native_own_property_is_sealed(
                    &body.length_property,
                    body.call_header.length_configurable(),
                )
                && crate::object::is_sealed(body.own_properties, heap)
        })
    }

    /// `Object.isFrozen` for native callable values.
    #[must_use]
    pub(crate) fn is_frozen(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| {
            !body.call_header.extensible()
                && native_own_property_is_frozen(
                    &body.name_property,
                    body.call_header.name_configurable(),
                )
                && native_own_property_is_frozen(
                    &body.length_property,
                    body.call_header.length_configurable(),
                )
                && crate::object::is_frozen(body.own_properties, heap)
        })
    }

    /// Return an own property descriptor for native function object
    /// metadata. Built-in `name` / `length` are non-writable,
    /// non-enumerable, configurable data properties.
    pub fn own_property_descriptor(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
    ) -> Result<Option<PropertyDescriptor>, otter_gc::OutOfMemory> {
        let mut external_visit = no_roots;
        self.own_property_descriptor_with_roots(heap, key, &mut external_visit)
    }

    fn own_property_descriptor_with_roots(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
        external_visit: &mut RootSlotVisitor<'_>,
    ) -> Result<Option<PropertyDescriptor>, otter_gc::OutOfMemory> {
        // Read property metadata under a shared payload borrow, then
        // build the descriptor outside the borrow so the heap remains
        // mutable for `native_builtin_descriptor`'s string alloc.
        enum Slot {
            Builtin,
            Deleted,
            Overridden(PropertyDescriptor),
            Expando(Option<PropertyDescriptor>),
        }
        let body_inner = self.inner;
        let slot = heap.read_payload(body_inner, |body| {
            let property = match key {
                "name" => &body.name_property,
                "length" => &body.length_property,
                _ => {
                    return Slot::Expando(crate::object::get_own_descriptor(
                        body.own_properties,
                        heap,
                        key,
                    ));
                }
            };
            match property {
                NativeOwnProperty::Builtin => Slot::Builtin,
                NativeOwnProperty::Deleted => Slot::Deleted,
                NativeOwnProperty::Overridden(desc) => Slot::Overridden(desc.clone()),
            }
        });
        match slot {
            Slot::Builtin => {
                let body_snapshot: NativeFunctionBodySnapshot =
                    heap.read_payload(body_inner, |body| NativeFunctionBodySnapshot {
                        name: body.name,
                        length: body.call_header.length,
                        name_configurable: body.call_header.name_configurable(),
                        length_configurable: body.call_header.length_configurable(),
                    });
                native_builtin_descriptor(&body_snapshot, heap, key, external_visit).map(Some)
            }
            Slot::Deleted => Ok(None),
            Slot::Overridden(d) => Ok(Some(d)),
            Slot::Expando(o) => Ok(o),
        }
    }

    /// Return an own symbol-keyed property descriptor stored on the
    /// native function object's ordinary property bag.
    pub(crate) fn own_symbol_property_descriptor(
        &self,
        heap: &otter_gc::GcHeap,
        key: crate::symbol::JsSymbol,
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

    /// Define or redefine one of the native function object's own properties.
    ///
    /// The target and descriptor are traced while materializing the built-in
    /// `name` descriptor, whose string allocation can move either value.
    ///
    /// `pub` because the `couch!` macro expands generated `install`
    /// bodies to pin static methods on the constructor through this
    /// method. Hand-written installers call it through the same
    /// path.
    pub fn define_own_property(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
        descriptor: PropertyDescriptor,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        self.define_own_property_partial(
            heap,
            key,
            PartialPropertyDescriptor::from_full(&descriptor),
        )
    }

    /// Rooted partial-descriptor entry used by the VM's
    /// `[[DefineOwnProperty]]` dispatcher.
    pub(crate) fn define_own_property_partial(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
        descriptor: PartialPropertyDescriptor,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        let mut target = Value::native_function(*self);
        let mut descriptor = descriptor;
        let mut roots = otter_gc::RootScope::new(heap);
        // SAFETY: these actual local slots remain stationary through descriptor
        // materialization and ordinary property-bag allocation.
        unsafe {
            crate::rooting::RootScopeExt::add_value(&mut roots, &mut target);
            crate::rooting::RootScopeExt::add_pelt(&mut roots, &mut descriptor);
        }
        let native = target
            .as_native_function()
            .expect("native function value must decode");
        let existing = {
            let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                target.trace_value_slot_mut(visitor);
                crate::pelt::PeltField::pelt_trace(&mut descriptor, visitor);
            };
            native.own_property_descriptor_with_roots(heap, key, &mut external_visit)?
        };
        let completed = match existing.as_ref() {
            Some(current) => descriptor.complete_against_current(current),
            None => descriptor.complete_for_new_property(),
        };
        let native = target
            .as_native_function()
            .expect("rooted native function value must decode");
        native.define_own_property_with_current(heap, key, existing, completed)
    }

    fn define_own_property_with_current(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
        existing: Option<PropertyDescriptor>,
        descriptor: PropertyDescriptor,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        let descriptor = match existing {
            Some(existing) => {
                match crate::object::validate_descriptor_update(&existing, &descriptor, heap) {
                    Some(merged) => merged,
                    None => return Ok(false),
                }
            }
            None if !self.is_extensible(heap) => return Ok(false),
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
        Ok(success)
    }

    /// Define or redefine a symbol-keyed own property on the native
    /// function object's ordinary property bag.
    pub(crate) fn define_own_symbol_property(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: crate::symbol::JsSymbol,
        descriptor: crate::object::PartialPropertyDescriptor,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        let mut owner = self.inner;
        let mut roots = otter_gc::RootScope::new(heap);
        // SAFETY: the actual old shell handle remains live through table growth.
        unsafe {
            roots.add_raw_slot(std::ptr::addr_of_mut!(owner).cast::<RawGc>());
        }
        let mut obj = heap.read_payload(owner, |body| body.own_properties);
        crate::object::define_own_symbol_property_partial(&mut obj, heap, key, descriptor)
    }

    /// Delete a configurable own metadata property.
    pub(crate) fn delete_own_property(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        if key != "name" && key != "length" {
            let mut owner = self.inner;
            let mut roots = otter_gc::RootScope::new(heap);
            // SAFETY: this old shell remains live through dictionary/table allocation.
            unsafe {
                roots.add_raw_slot(std::ptr::addr_of_mut!(owner).cast::<RawGc>());
            }
            let mut own_properties = heap.read_payload(owner, |body| body.own_properties);
            return crate::object::delete(&mut own_properties, heap, key);
        }
        Ok(heap.with_payload(self.inner, |body| {
            let slot = match key {
                "name" => &mut body.name_property,
                "length" => &mut body.length_property,
                _ => return true,
            };
            let configurable = match slot {
                NativeOwnProperty::Builtin => match key {
                    "name" => body.call_header.name_configurable(),
                    "length" => body.call_header.length_configurable(),
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
        }))
    }

    /// Delete a configurable symbol-keyed own property from the
    /// native function object's ordinary property bag.
    pub(crate) fn delete_own_symbol_property(
        &self,
        heap: &mut otter_gc::GcHeap,
        key: crate::symbol::JsSymbol,
    ) -> bool {
        let own_properties = heap.read_payload(self.inner, |body| body.own_properties);
        crate::object::delete_symbol(own_properties, heap, key)
    }

    /// Clone the call target so the caller can invoke it after
    /// releasing the heap borrow. Captures stay in their slab: the
    /// target carries the handle, and the invoked closure reads the
    /// live storage — which a collection mid-call rewrites in place,
    /// where a stack clone would silently go stale.
    #[must_use]
    pub(crate) fn call_target(&self, heap: &otter_gc::GcHeap) -> NativeCallTarget {
        heap.read_payload(self.inner, |body| {
            body.call_header.target(heap, body.captures)
        })
    }

    /// `true` when this callable uses the static function-pointer
    /// fast path.
    #[must_use]
    pub fn is_static_call(&self, heap: &otter_gc::GcHeap) -> bool {
        heap.read_payload(self.inner, |body| {
            body.call_header.kind == NativeEntryKind::Static
        })
    }

    /// `true` when this callable is the exact static native function.
    #[must_use]
    /// Static entry of a static-backed callable, read once.
    pub(crate) fn static_fn(&self, heap: &otter_gc::GcHeap) -> Option<NativeFastFn> {
        heap.read_payload(self.inner, |body| body.call_header.static_fn())
    }

    pub(crate) fn is_static_fn(&self, heap: &otter_gc::GcHeap, expected: NativeFastFn) -> bool {
        self.static_fn(heap)
            .is_some_and(|call| std::ptr::fn_addr_eq(call, expected))
    }

    /// External-reference index identifying this callable's static entry or
    /// VM intrinsic for JIT builtin guards. `None` for a dynamic closure.
    #[must_use]
    pub(crate) fn native_ref(&self, heap: &otter_gc::GcHeap) -> Option<u32> {
        heap.read_payload(self.inner, |body| {
            (body.call_header.native_ref != otter_gc::NO_EXTERNAL_REF)
                .then_some(body.call_header.native_ref)
        })
    }

    /// `true` when this callable resolves to the named VM intrinsic.
    /// Used by §13.10.2 InstanceofOperator's fast path so the spec's
    /// `Call(Function.prototype[@@hasInstance], target, « V »)`
    /// dispatches straight to OrdinaryHasInstance instead of pushing
    /// an extra frame.
    #[must_use]
    pub fn is_vm_intrinsic(&self, heap: &otter_gc::GcHeap, intrinsic: VmIntrinsicFunction) -> bool {
        heap.read_payload(self.inner, |body| {
            body.call_header.intrinsic() == Some(intrinsic)
        })
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
    /// Static function with traced captures.
    StaticWithCaptures {
        /// Entry point.
        call: NativeCapturesFn,
        /// The callee body's capture slab.
        captures: crate::value_slab::ValueSlabHandle,
    },
    /// VM-owned intrinsic function.
    VmIntrinsic(VmIntrinsicFunction),
    /// Dynamic closure path with traced captures.
    Dynamic {
        /// Closure payload.
        call: Arc<NativeFn>,
        /// The callee body's capture slab.
        captures: crate::value_slab::ValueSlabHandle,
    },
    /// Local VM-only closure path.
    LocalDynamic {
        /// Closure payload.
        call: Arc<LocalNativeFn>,
        /// The callee body's capture slab.
        captures: crate::value_slab::ValueSlabHandle,
    },
}

impl NativeCallTarget {
    /// Invoke the target.
    ///
    /// The captures slice aliases the callee's live slab storage: the
    /// callee is rooted for the duration of its own call, the slab is
    /// old space and does not move, and a collection mid-call rewrites
    /// the slots in place — so the closure always reads current
    /// handles, never a pre-move copy.
    pub(crate) fn invoke(
        self,
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        match self {
            Self::Static(call) => call(ctx, args),
            // SAFETY: see the doc above — the callee roots the slab
            // across the call.
            Self::StaticWithCaptures { call, captures } => call(ctx, args, unsafe {
                crate::value_slab::live_slice(captures)
            }),
            Self::VmIntrinsic(intrinsic) => Err(NativeError::TypeError {
                name: intrinsic.name(),
                reason: "VM intrinsic requires interpreter dispatch".to_string(),
            }),
            // SAFETY: see the doc above — the callee roots the slab
            // across the call.
            Self::Dynamic { call, captures } => call(ctx, args, unsafe {
                crate::value_slab::live_slice(captures)
            }),
            Self::LocalDynamic { call, captures } => call(ctx, args, unsafe {
                crate::value_slab::live_slice(captures)
            }),
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
    Ok(Value::native_function(NativeFunction::new(
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
    Ok(Value::native_function(NativeFunction::new_static(
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
    Ok(Value::native_function(NativeFunction::with_captures(
        heap, name, call, captures,
    )?))
}

/// Root-aware counterpart used by [`crate::NativeCtx`].
///
/// The closure remains `Send + Sync`; only the caller's runtime root walk is
/// supplied separately. This keeps the public native-binding path on the safe
/// dynamic-call representation instead of exposing the VM-internal
/// `LocalDynamic` escape hatch.
pub(crate) fn native_value_with_captures_and_roots<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    captures: SmallVec<[Value; 4]>,
    external_visit: &mut RootSlotVisitor<'_>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError>
        + Send
        + Sync
        + 'static,
{
    Ok(Value::native_function(NativeFunction::allocate_with_roots(
        heap,
        name,
        length,
        NativeCallStorage::Dynamic(Arc::new(call)),
        captures,
        NativeFunctionMetadata::BUILTIN,
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
    Ok(Value::native_function(NativeFunction::allocate_with_roots(
        heap,
        name,
        0,
        NativeCallStorage::LocalDynamic(Arc::new(call)),
        captures,
        NativeFunctionMetadata::BUILTIN,
        external_visit,
    )?))
}

pub(crate) fn local_native_value_with_length<F>(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    captures: SmallVec<[Value; 4]>,
    external_visit: &mut RootSlotVisitor<'_>,
    call: F,
) -> Result<Value, otter_gc::OutOfMemory>
where
    F: for<'rt> Fn(&mut NativeCtx<'rt>, &[Value], &[Value]) -> Result<Value, NativeError> + 'static,
{
    Ok(Value::native_function(NativeFunction::allocate_with_roots(
        heap,
        name,
        length,
        NativeCallStorage::LocalDynamic(Arc::new(call)),
        captures,
        NativeFunctionMetadata::BUILTIN,
        external_visit,
    )?))
}

struct NativeFunctionBodySnapshot {
    name: JsString,
    length: u8,
    name_configurable: bool,
    length_configurable: bool,
}

fn native_builtin_descriptor(
    body: &NativeFunctionBodySnapshot,
    _heap: &mut otter_gc::GcHeap,
    key: &str,
    _external_visit: &mut RootSlotVisitor<'_>,
) -> Result<PropertyDescriptor, otter_gc::OutOfMemory> {
    let value = match key {
        // The body already owns its name as a heap string; hand the
        // handle out directly instead of allocating a copy.
        "name" => Value::string(body.name),
        "length" => Value::number(crate::number::NumberValue::from_i32(body.length as i32)),
        _ => Value::undefined(),
    };
    let configurable = match key {
        "name" => body.name_configurable,
        "length" => body.length_configurable,
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

fn native_own_property_is_sealed(property: &NativeOwnProperty, builtin_configurable: bool) -> bool {
    match property {
        NativeOwnProperty::Builtin => !builtin_configurable,
        NativeOwnProperty::Deleted => true,
        NativeOwnProperty::Overridden(desc) => !desc.flags.configurable(),
    }
}

fn native_own_property_is_frozen(property: &NativeOwnProperty, builtin_configurable: bool) -> bool {
    match property {
        NativeOwnProperty::Builtin => !builtin_configurable,
        NativeOwnProperty::Deleted => true,
        NativeOwnProperty::Overridden(desc) => {
            !desc.flags.configurable() && (!desc.is_data() || !desc.flags.writable())
        }
    }
}

/// Failure outcome from a native call. The runtime mapper routes
/// these outcomes through the same VM error path as bytecode throws
/// and allocation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NativeError {
    /// A completed VM failure imported by synchronous re-entry or marshalling.
    /// Only structural/control failure and escaping OOM are legal here; a
    /// catchable value is already represented by the existing throw owner.
    /// Owned detail and source frames survive nested native/Wasm transport.
    #[error(transparent)]
    ExecutionFailure(crate::RunError),
    /// Malformed bytecode or an invalid private engine operand. Never catchable.
    #[error("invalid operand")]
    InvalidOperand,
    /// A bytecode body completed without its required return. Never catchable.
    #[error("function did not RETURN")]
    MissingReturn,
    /// Exact engine resource-admission refusal. Never a JavaScript throw.
    /// Carries the existing ledger payload unchanged across owned boundaries.
    #[error("{error}")]
    Resource {
        /// Actual resource class, requested amount and physical headroom.
        error: otter_resource::ResourceError,
    },
    /// A user-thrown JS value escaped the native body. The
    /// dispatcher will route this through the same path as
    /// `Op::Throw` — i.e. into the function's handler table.
    #[error("native function {name} threw: {message}")]
    Thrown {
        /// Display name of the offending native (for diagnostics).
        name: &'static str,
        /// Owned host diagnostic text. A JavaScript value thrown during VM
        /// reentry retains its identity in the interpreter's one traced
        /// pending-throw owner during immediate synchronous propagation only.
        /// A deferred worker result transports this text, not that JS identity;
        /// this payload never carries a raw GC handle.
        message: String,
    },
    /// A host ABI requested a catchable ordinary JavaScript `Error` with an
    /// already-finalized message (for example Node-API's `napi_throw_error`).
    #[error("{message}")]
    Error {
        /// Human-readable error message.
        message: String,
    },
    /// A spec-mandated error whose message is already exactly what the
    /// language prescribes. The class-named variants below prefix the
    /// throwing native's name, which reads well for engine diagnostics
    /// but corrupts a message the specification quotes verbatim.
    #[error("{message}")]
    SpecError {
        /// JS error class for the thrown instance.
        kind: crate::error_classes::ErrorKind,
        /// The complete message, used unchanged.
        message: String,
    },
    /// A JS error carrying a Node-style `.code` (`ERR_*`). `kind` selects the
    /// error class; `code` becomes an own property on the thrown instance.
    /// Native modules use this for structured `error.code` (no string munging).
    #[error("{message}")]
    Coded {
        /// JS error class for the thrown instance.
        kind: crate::error_classes::ErrorKind,
        /// Stable Node error code (`"ERR_*"`).
        code: &'static str,
        /// Human-readable message.
        message: String,
    },
    /// A failed system call, carrying the own properties Node stamps on one:
    /// `code`, `errno`, `syscall`, and the path operands the call was given.
    /// Node's own tests read these off `fs`, `net`, and `process` errors, so a
    /// native that wraps a syscall reports it here rather than as a bare
    /// [`NativeError::Coded`], which carries only a code.
    #[error("{message}")]
    Syscall {
        /// Node error code for the failure (`"ENOENT"`).
        code: &'static str,
        /// Rendered message, already in Node's
        /// `CODE: description, syscall 'path'` shape.
        message: String,
        /// Name of the call that failed (`"chdir"`, `"open"`, `"kill"`).
        syscall: &'static str,
        /// Primary path operand, when the call had one.
        path: Option<String>,
        /// Secondary path operand, for calls that take two (`rename`, `chdir`).
        dest: Option<String>,
        /// Platform errno, reported negated the way Node reports it.
        errno: i32,
    },
    /// Type or value error inside the native body that does not
    /// originate as a `throw` (e.g. wrong arity). The dispatcher constructs
    /// its intrinsic TypeError and routes the value through the normal throw
    /// owner; allocation failure remains its exact typed cause.
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
    /// Malformed input to `decodeURI*` / `encodeURI*`; surfaces as a JS
    /// `URIError` (§19.2.6).
    #[error("native function {name}: {reason}")]
    URIError {
        /// Display name of the native.
        name: &'static str,
        /// Short reason.
        reason: String,
    },
    /// Access to an uninitialized binding (Temporal Dead Zone) or an
    /// unresolved reference surfaced from inside a native. §13.3.7.3 /
    /// §10.2.2 — a JS `ReferenceError`. Without this variant a TDZ
    /// error raised behind a native boundary (e.g. a module namespace
    /// MOP reached through `Object.keys`) would be misreported as a
    /// `TypeError`.
    #[error("native function {name}: {reason}")]
    ReferenceError {
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
    /// Host-visible runtime interruption observed inside a blocking
    /// native operation. This is not a JS throw and must not be
    /// catchable by user code.
    #[error("native function interrupted")]
    Interrupted,
    /// Structural resource exhaustion raised while a native operation was
    /// executing. This is not a JavaScript throw and must retain the VM's
    /// `BudgetExceeded` identity across the native boundary.
    #[error("native function exceeded work budget: {reason}")]
    BudgetExceeded {
        /// Human-readable resource that exhausted its finite budget.
        reason: String,
    },
    /// Explicitly authored allocation-failure data from a native body.
    /// With diagnostic headroom this projects once to a catchable RangeError.
    /// Imported VM allocation or completed reentry failures use ExecutionFailure
    /// and cannot start another JavaScript error build.
    #[error("native function {name}: out of memory")]
    OutOfMemory {
        /// Display name of the native.
        name: &'static str,
        /// Bytes the failing allocation requested.
        requested_bytes: u64,
        /// Configured heap limit in bytes.
        heap_limit_bytes: u64,
    },
}

impl NativeError {
    /// Pure completion classification, with no allocation or VM mutation.
    /// An invalid ExecutionFailure payload is itself a structural rejection;
    /// the canonical VM projection validates its exact owned discriminant.
    #[must_use]
    pub const fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Resource { .. }
                | Self::MissingReturn
                | Self::InvalidOperand
                | Self::Interrupted
                | Self::BudgetExceeded { .. }
                | Self::Exit { .. }
                | Self::ExecutionFailure(_)
        )
    }

    /// Pure control payload access after a completed native call.
    ///
    /// Imported execution failures retain their exact VM exit code. This does
    /// not classify or materialize catchable errors; disposition remains owned
    /// by `is_fatal` and the validated ExecutionFailure transport.
    #[must_use]
    pub fn exit_code(&self) -> Option<u8> {
        match self {
            Self::Exit { code } => Some(*code),
            Self::ExecutionFailure(failure) => match failure.error {
                crate::VmError::Exit { code } => Some(code),
                _ => None,
            },
            _ => None,
        }
    }
}

// The owned completion boundary transports no VM handles or thread-local state.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NativeError>();
    assert_send_sync::<crate::RunError>();
};

impl From<otter_gc::OutOfMemory> for NativeError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory {
            name: "native",
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }
}

/// Map a re-entry [`crate::VmError`] onto the native error model.
///
/// `VmError::Uncaught` carries a user-thrown JS value and is preserved
/// as [`NativeError::Thrown`] so callbacks that `throw` surface intact;
/// the spec error classes map to their `NativeError` counterparts and
/// structural failures and runtime control keep their exact identity. Each
/// catchable VM family selects its specified error class explicitly.
pub fn vm_to_native_error(
    interp: &crate::Interpreter,
    err: crate::VmError,
    name: &'static str,
) -> NativeError {
    use crate::run_control::ErrorDetail;
    // The dynamic message/payload for an in-flight error lives in the isolate's
    // pending-error slot (`VmError` is `Copy`); pull it out here paired with the
    // `Copy` discriminant.
    let detail = interp.error_detail();
    let message = || match &detail {
        Some(ErrorDetail::Message(m)) => m.to_string(),
        Some(ErrorDetail::Name(m)) => m.to_string(),
        Some(ErrorDetail::Uncaught(m)) => m.to_string(),
        _ => err.to_string(),
    };
    match err {
        // ResourceLimit and its exact detail are published together. Missing
        // or wrong detail is malformed engine state, never a fabricated ledger
        // cause or a catchable JavaScript error.
        crate::VmError::ResourceLimit => match detail {
            Some(ErrorDetail::Resource(error)) => NativeError::Resource { error },
            _ => NativeError::InvalidOperand,
        },
        crate::VmError::MissingReturn => NativeError::MissingReturn,
        crate::VmError::InvalidOperand => NativeError::InvalidOperand,
        crate::VmError::Uncaught => NativeError::Thrown {
            name,
            message: message(),
        },
        crate::VmError::Coded => match detail {
            Some(ErrorDetail::Syscall(payload)) => NativeError::Syscall {
                code: payload.code,
                message: payload.message,
                syscall: payload.syscall,
                path: payload.path,
                dest: payload.dest,
                errno: payload.errno,
            },
            Some(ErrorDetail::Coded(payload)) => NativeError::Coded {
                kind: payload.kind,
                code: payload.code,
                message: payload.message,
            },
            _ => NativeError::SpecError {
                kind: crate::ErrorKind::Error,
                message: err.to_string(),
            },
        },
        crate::VmError::TypeError => NativeError::TypeError {
            name,
            reason: message(),
        },
        crate::VmError::TypeMismatchAt => NativeError::TypeError {
            name,
            reason: interp.render_vm_error(&err),
        },
        crate::VmError::RangeError => NativeError::RangeError {
            name,
            reason: message(),
        },
        crate::VmError::SyntaxError => NativeError::SyntaxError {
            name,
            reason: message(),
        },
        // §10.2.2 / §13.3.7.3 — TDZ and unresolved-reference errors are
        // ReferenceErrors and must keep that class across the native
        // boundary rather than collapsing to the TypeError fallback.
        crate::VmError::ThisUninitialized => NativeError::ReferenceError {
            name,
            reason: message(),
        },
        crate::VmError::TemporalDeadZone { .. } | crate::VmError::UndefinedIdentifier => {
            NativeError::ReferenceError {
                name,
                reason: message(),
            }
        }
        crate::VmError::Interrupted => NativeError::Interrupted,
        crate::VmError::BudgetExceeded => NativeError::BudgetExceeded { reason: message() },
        // A failure that already escaped VM execution is final. Importing it
        // must not turn it into an authored allocation OOM and initiate a
        // second JavaScript error allocation in the enclosing native body.
        crate::VmError::OutOfMemory { .. } => NativeError::ExecutionFailure(crate::RunError {
            error: err,
            frames: interp.pending_uncaught_frames.clone().unwrap_or_default(),
            // OOM has only its exact scalar cause; older dynamic detail cannot
            // describe this allocation refusal.
            detail: None,
        }),
        // `process.exit(code)` must keep its identity across the native
        // boundary so a host (e.g. the CommonJS loader) can surface a clean
        // process termination instead of an uncatchable-looking TypeError.
        crate::VmError::Exit { code } => NativeError::Exit { code },
        // URIError / UnknownIntrinsic / InvalidRegExp are
        // raised with a message or name detail of their own.
        crate::VmError::URIError => NativeError::URIError {
            name,
            reason: message(),
        },
        crate::VmError::InvalidRegExp => NativeError::SyntaxError {
            name,
            reason: message(),
        },
        crate::VmError::JsonError => {
            let (kind, reason) = match detail {
                Some(ErrorDetail::Json(payload)) => (
                    if payload.code == "JSON_PARSE" {
                        crate::ErrorKind::SyntaxError
                    } else {
                        crate::ErrorKind::TypeError
                    },
                    payload.message,
                ),
                _ => (crate::ErrorKind::TypeError, err.to_string()),
            };
            NativeError::SpecError {
                kind,
                message: reason,
            }
        }
        crate::VmError::UnknownIntrinsic => NativeError::TypeError {
            name,
            reason: message(),
        },
        // Everything else raises no detail. The isolate's slot holds one
        // error's detail at a time and is only emptied where an error
        // surfaces, so reading it here would pin whatever was raised last
        // onto this error — and, since the result is raised in turn, the next
        // detail-less error would inherit that, and the one after it both.
        // Each renders its own text instead.
        crate::VmError::StackOverflow { limit } => NativeError::RangeError {
            name,
            reason: format!("maximum call stack size exceeded (limit {limit})"),
        },
        crate::VmError::TypeMismatch | crate::VmError::NotCallable => NativeError::TypeError {
            name,
            reason: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NativeCallInfo;

    #[test]
    fn c_header_selects_every_native_payload_and_preserves_callable_policy() {
        fn plain(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args[0])
        }
        fn captured(
            _: &mut NativeCtx<'_>,
            _: &[Value],
            captures: &[Value],
        ) -> Result<Value, NativeError> {
            Ok(captures[0])
        }
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let layout = jit_call_layout();
        let cases = [
            (NativeEntryKind::Static, NativeCallStorage::Static(plain)),
            (
                NativeEntryKind::StaticWithCaptures,
                NativeCallStorage::StaticWithCaptures(captured),
            ),
            (
                NativeEntryKind::VmIntrinsic,
                NativeCallStorage::VmIntrinsic(VmIntrinsicFunction::FunctionPrototypeApply),
            ),
            (
                NativeEntryKind::Dynamic,
                NativeCallStorage::Dynamic(Arc::new(|_, _, captures| Ok(captures[0]))),
            ),
            (
                NativeEntryKind::LocalDynamic,
                NativeCallStorage::LocalDynamic(Arc::new(|_, _, captures| Ok(captures[0]))),
            ),
        ];
        for (kind, storage) in cases {
            let mut roots = no_roots;
            let native = NativeFunction::allocate_with_roots(
                interp.gc_heap_mut(),
                "header-probe",
                3,
                storage,
                smallvec::smallvec![Value::number_i32(23)],
                NativeFunctionMetadata::CONSTRUCTOR,
                &mut roots,
            )
            .expect("native callable");
            interp.gc_heap().read_payload(native.inner, |body| {
                // SAFETY: this live payload borrow retains its cell. The VM
                // publishes these exact aligned offsets to both JIT encoders.
                let cell = unsafe {
                    std::ptr::from_ref(body)
                        .cast::<u8>()
                        .sub(otter_gc::header::HEADER_SIZE)
                };
                let raw_kind = unsafe { *cell.add(layout.kind_byte as usize) };
                let flags = unsafe { *cell.add(layout.flags_byte as usize) };
                let payload =
                    unsafe { cell.add(layout.payload_byte as usize).cast::<u64>().read() };
                let identity =
                    unsafe { cell.add(layout.identity_byte as usize).cast::<u32>().read() };
                let captures =
                    unsafe { cell.add(layout.captures_byte as usize).cast::<u32>().read() };
                assert_eq!(raw_kind, kind as u8);
                assert_ne!(captures, 0);
                assert_eq!(identity, body.call_header.native_ref);
                assert_ne!(flags & layout.constructable_flag, 0);
                assert_ne!(flags & layout.extensible_flag, 0);
                match kind {
                    NativeEntryKind::Static => assert_eq!(payload, plain as *const () as u64),
                    NativeEntryKind::StaticWithCaptures => {
                        assert_eq!(payload, captured as *const () as u64)
                    }
                    NativeEntryKind::VmIntrinsic => {
                        assert_eq!(payload, VmIntrinsicFunction::FunctionPrototypeApply as u64)
                    }
                    NativeEntryKind::Dynamic | NativeEntryKind::LocalDynamic => {
                        assert_eq!(payload >> 32, 0);
                        assert!(interp.gc_heap().host_refs().get(payload as u32).is_some());
                    }
                }
            });
            assert_eq!(native.length(interp.gc_heap()), 3);
            assert!(native.is_constructable(interp.gc_heap()));
            let call = native.call_target(interp.gc_heap());
            if kind == NativeEntryKind::VmIntrinsic {
                assert!(matches!(
                    call,
                    NativeCallTarget::VmIntrinsic(VmIntrinsicFunction::FunctionPrototypeApply)
                ));
            } else {
                NativeCtx::with_host_context(
                    &mut interp,
                    NativeCallInfo::call(Value::UNDEFINED),
                    None,
                    |ctx| {
                        let result = call.invoke(ctx, &[Value::number_i32(7)]).unwrap();
                        assert_eq!(
                            result,
                            Value::number_i32(if kind == NativeEntryKind::Static {
                                7
                            } else {
                                23
                            })
                        );
                    },
                );
            }
            native
                .prevent_extensions(interp.gc_heap_mut())
                .expect("native fixture prevent extensions");
            assert!(!native.is_extensible(interp.gc_heap()));
            assert!(native.is_constructable(interp.gc_heap()));
            assert_eq!(native.length(interp.gc_heap()), 3);
        }
    }

    #[test]
    fn restricted_native_header_preserves_metadata_descriptor_rules() {
        fn entry(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> {
            Ok(Value::UNDEFINED)
        }
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let mut roots = no_roots;
        let native =
            NativeFunction::throw_type_error_with_roots(interp.gc_heap_mut(), entry, &mut roots)
                .expect("restricted native");
        assert!(!native.is_constructable(interp.gc_heap()));
        assert!(!native.is_extensible(interp.gc_heap()));
        for property in ["name", "length"] {
            let descriptor = native
                .own_property_descriptor(interp.gc_heap_mut(), property)
                .unwrap()
                .unwrap();
            assert!(!descriptor.configurable());
            assert!(
                !native
                    .delete_own_property(interp.gc_heap_mut(), property)
                    .expect("restricted descriptor deletion")
            );
        }
    }

    /// A dynamic native's closure lives in the host-ref table; the
    /// slot must be released when the body dies — young (scavenge) or
    /// old (full sweep) — and survive while the body is rooted.
    #[test]
    fn dynamic_native_host_ref_slot_is_released_when_the_body_dies() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let baseline = interp.gc_heap().host_refs().len();
        let f = native_value(interp.gc_heap_mut(), "closure", |_, _, _| {
            Ok(Value::undefined())
        })
        .expect("native");
        assert_eq!(
            interp.gc_heap().host_refs().len(),
            baseline + 1,
            "allocation interned the closure"
        );
        // Rooted across a full collection: the slot must survive. The
        // native rides a global handle so the interpreter's own root
        // walk keeps it, alongside the bootstrap graph.
        interp
            .set_global("__host_ref_probe", f)
            .expect("fixture global descriptor");
        interp.force_gc().expect("force GC");
        assert_eq!(interp.gc_heap().host_refs().len(), baseline + 1);
        // Unrooted: the next full collection reclaims the body and the
        // slot, while the bootstrap graph's own entries stay put.
        interp
            .set_global("__host_ref_probe", Value::undefined())
            .expect("fixture global descriptor");
        interp.force_gc().expect("force GC");
        assert_eq!(
            interp.gc_heap().host_refs().len(),
            baseline,
            "dead body released its host-ref slot"
        );
    }

    /// The restore-path clone lands every closure at its original
    /// index, so restored bodies resolve without rewriting.
    #[test]
    fn host_ref_clone_preserves_indices() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let _f = native_value(interp.gc_heap_mut(), "cloned", |_, _, _| {
            Ok(Value::undefined())
        })
        .expect("native");
        let mut target = otter_gc::GcHeap::new().expect("target heap");
        clone_host_refs_for_restore(interp.gc_heap(), &mut target);
        assert_eq!(
            target.host_refs().len(),
            interp.gc_heap().host_refs().len(),
            "every closure entry cloned"
        );
        let source_indices: Vec<u32> = interp
            .gc_heap()
            .host_refs()
            .entries()
            .map(|(i, _)| i)
            .collect();
        let target_indices: Vec<u32> = target.host_refs().entries().map(|(i, _)| i).collect();
        assert_eq!(source_indices, target_indices, "indices preserved");
    }

    #[test]
    fn native_value_dispatches() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let f = native_value(interp.gc_heap_mut(), "identity", |_, args, _captures| {
            Ok(args.first().cloned().unwrap_or(Value::undefined()))
        })
        .expect("native");
        let native = f.as_native_function().expect("expected NativeFunction");
        let call = native.call_target(interp.gc_heap());
        NativeCtx::with_host_context(
            &mut interp,
            NativeCallInfo::call(Value::undefined()),
            None,
            |ctx| {
                let r = call.invoke(ctx, &[Value::number_i32(7)]).unwrap();
                assert_eq!(r.display_string(ctx.heap()), "7");
            },
        );
    }

    #[test]
    fn rejects_arity_via_typeerror() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
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
                Ok(args[0])
            },
        )
        .expect("native");
        let native = f.as_native_function().expect("NativeFunction");
        let call = native.call_target(interp.gc_heap());
        NativeCtx::with_host_context(
            &mut interp,
            NativeCallInfo::call(Value::undefined()),
            None,
            |ctx| {
                let err = call.invoke(ctx, &[]).unwrap_err();
                assert!(matches!(err, NativeError::TypeError { .. }));
            },
        );
    }

    #[test]
    fn static_native_value_uses_fast_path_and_length() {
        fn id(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            Ok(args.first().cloned().unwrap_or(Value::undefined()))
        }

        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        let f = native_value_static(interp.gc_heap_mut(), "id", 1, id).expect("native");
        let native = f.as_native_function().expect("expected NativeFunction");
        assert!(native.is_static_call(interp.gc_heap()));
        assert_eq!(native.length(interp.gc_heap()), 1);
    }
}
