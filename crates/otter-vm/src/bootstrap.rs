//! Centralized bootstrap registry for global JavaScript surfaces.
//!
//! The registry owns the deterministic install order for globals,
//! constructors, namespaces, and host-provided surfaces. Entries
//! install through static specs and builders when available,
//! or through small placeholder installers for surfaces that have not
//! been migrated yet.
//!
//! # Contents
//! - [`BootstrapFeatures`] — install-time feature/capability gates.
//! - [`BootstrapTelemetry`] — opt-in startup/bench counters.
//! - [`BootstrapEntry`] / [`BOOTSTRAP_ENTRIES`] — deterministic
//!   registry data.
//! - [`build_global_this`] — one-shot global object bootstrap.
//!
//! # Invariants
//! - The registry is a static ordered slice, not a hash map or
//!   closure registry in the JS hot path.
//! - Installers receive an explicit `&mut GcHeap` and global object.
//! - Duplicate global names are rejected by tests.
//! - Feature gates are checked at install time before allocation for
//!   an entry.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-global-object>
//! - [JS surface builders](../../../docs/book/src/extensions/js-surface-builders.md)

use std::time::{Duration, Instant};

use crate::js_surface::{Attr, JsSurfaceError, NamespaceSpec, ObjectBuilder};
use crate::number::NumberValue;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{
    NativeCtx, NativeError, Value, VmGetOutcome, VmPropertyKey, array, array_prototype,
    array_statics, atomics, console, constructor_return_is_object, descriptor_value,
    function_prototype, json, math, object_statics, reflect,
};
use smallvec::SmallVec;

pub(crate) fn alloc_object_with_value_roots(
    heap: &mut otter_gc::GcHeap,
    value_roots: &[&Value],
) -> Result<JsObject, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
    };
    object::alloc_object_with_roots(heap, &mut external_visit)
}

pub(crate) fn native_constructor_static_with_value_roots(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    value_roots: &[&Value],
) -> Result<crate::native_function::NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
    };
    crate::native_function::NativeFunction::new_constructor_static_with_roots(
        heap,
        name,
        length,
        call,
        &mut external_visit,
    )
}

pub(crate) fn native_new_target_prototype(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
) -> Result<Option<Value>, NativeError> {
    let Some(new_target) = ctx.new_target().cloned() else {
        return Ok(None);
    };
    let proto = if let Some(exec) = ctx.execution_context().cloned() {
        let key = VmPropertyKey::String("prototype");
        let (interp, _) = ctx.interp_mut_and_context();
        match interp
            .ordinary_get_value(&exec, new_target.clone(), new_target.clone(), &key, 0)
            .map_err(|err| NativeError::TypeError {
                name,
                reason: err.to_string(),
            })? {
            VmGetOutcome::Value(value) => Some(value),
            VmGetOutcome::InvokeGetter { getter } => Some(
                interp
                    .run_callable_sync(&exec, &getter, new_target, SmallVec::new())
                    .map_err(|err| native_new_target_error(name, err))?,
            ),
        }
    } else {
        match new_target {
            Value::ClassConstructor(class) => Some(Value::Object(class.prototype(ctx.heap()))),
            Value::Object(obj) => object::get(obj, ctx.heap(), "prototype"),
            Value::NativeFunction(native) => native
                .own_property_descriptor(ctx.heap(), ctx.cx.interp.string_heap(), "prototype")
                .map_err(|err| NativeError::TypeError {
                    name,
                    reason: err.to_string(),
                })?
                .map(|descriptor| descriptor_value(&descriptor)),
            _ => None,
        }
    };
    Ok(proto
        .filter(|value| constructor_return_is_object(value) || matches!(value, Value::Proxy(_))))
}

fn native_new_target_error(name: &'static str, err: crate::VmError) -> NativeError {
    match err {
        crate::VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}

pub(crate) fn native_static_with_value_roots(
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    length: u8,
    call: crate::native_function::NativeFastFn,
    value_roots: &[&Value],
) -> Result<crate::native_function::NativeFunction, otter_gc::OutOfMemory> {
    let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        for value in value_roots {
            value.trace_value_slots(visitor);
        }
    };
    crate::native_function::NativeFunction::new_static_with_roots(
        heap,
        name,
        length,
        call,
        &mut external_visit,
    )
}

/// Bootstrap feature/capability bitset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapFeatures(u32);

impl BootstrapFeatures {
    /// Core ECMAScript placeholder/builtin globals.
    pub const CORE: Self = Self(0b0001);
    /// Host console surface.
    pub const CONSOLE: Self = Self(0b0010);

    /// All default surfaces.
    #[must_use]
    pub const fn all() -> Self {
        Self(Self::CORE.0 | Self::CONSOLE.0)
    }

    /// Empty feature set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// `true` when all `required` bits are enabled.
    #[must_use]
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Return a copy without `feature`.
    #[must_use]
    pub const fn without(self, feature: Self) -> Self {
        Self(self.0 & !feature.0)
    }
}

/// One deterministic bootstrap entry.
pub struct BootstrapEntry {
    /// Global name installed by this entry.
    pub name: &'static str,
    /// Required feature/capability bits.
    pub feature: BootstrapFeatures,
    /// Installer function.
    pub install: BootstrapInstall,
}

/// Bootstrap installer function pointer.
pub type BootstrapInstall =
    fn(&BootstrapEntry, &mut otter_gc::GcHeap, JsObject) -> Result<(), JsSurfaceError>;

/// One timed bootstrap phase captured by [`BootstrapTelemetry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapPhaseTelemetry {
    /// Phase label. Registry entry phases use the installed global name.
    pub name: &'static str,
    /// Wall-clock time spent in this phase.
    pub duration: Duration,
}

/// Default-off bootstrap counters for startup benchmarks and ratchets.
///
/// The normal runtime construction path does not allocate or update this
/// structure. Use [`build_global_this_with_telemetry`] from benches or focused
/// tests when a bootstrap change needs measured install counts.
#[derive(Debug, Clone, Default)]
pub struct BootstrapTelemetry {
    entries_considered: usize,
    entries_installed: usize,
    entries_skipped: usize,
    objects_installed: usize,
    prototype_objects_installed: usize,
    namespace_objects_installed: usize,
    native_functions_installed: usize,
    strings_interned: usize,
    gc_allocations: u64,
    gc_allocated_bytes: usize,
    duplicate_name_checks: usize,
    duplicate_names_found: usize,
    phases: Vec<BootstrapPhaseTelemetry>,
}

impl BootstrapTelemetry {
    /// Registry entries visited by the selected feature set.
    #[must_use]
    pub const fn entries_considered(&self) -> usize {
        self.entries_considered
    }

    /// Registry entries whose installer ran.
    #[must_use]
    pub const fn entries_installed(&self) -> usize {
        self.entries_installed
    }

    /// Registry entries skipped by feature gates.
    #[must_use]
    pub const fn entries_skipped(&self) -> usize {
        self.entries_skipped
    }

    /// Ordinary JS objects installed during bootstrap, including prototypes.
    #[must_use]
    pub const fn objects_installed(&self) -> usize {
        self.objects_installed
    }

    /// Prototype objects installed for placeholder constructor-shaped globals.
    #[must_use]
    pub const fn prototype_objects_installed(&self) -> usize {
        self.prototype_objects_installed
    }

    /// Namespace objects installed from static specs.
    #[must_use]
    pub const fn namespace_objects_installed(&self) -> usize {
        self.namespace_objects_installed
    }

    /// Native function objects installed from static specs.
    #[must_use]
    pub const fn native_functions_installed(&self) -> usize {
        self.native_functions_installed
    }

    /// Strings interned by the bootstrap path.
    #[must_use]
    pub const fn strings_interned(&self) -> usize {
        self.strings_interned
    }

    /// GC allocation count delta observed during bootstrap.
    #[must_use]
    pub const fn gc_allocations(&self) -> u64 {
        self.gc_allocations
    }

    /// GC live-byte delta observed during bootstrap.
    #[must_use]
    pub const fn gc_allocated_bytes(&self) -> usize {
        self.gc_allocated_bytes
    }

    /// Number of registry names checked for duplicates.
    #[must_use]
    pub const fn duplicate_name_checks(&self) -> usize {
        self.duplicate_name_checks
    }

    /// Duplicate registry names found during telemetry validation.
    #[must_use]
    pub const fn duplicate_names_found(&self) -> usize {
        self.duplicate_names_found
    }

    /// Timed bootstrap phases in execution order.
    #[must_use]
    pub fn phases(&self) -> &[BootstrapPhaseTelemetry] {
        &self.phases
    }

    fn reset(&mut self) {
        self.entries_considered = 0;
        self.entries_installed = 0;
        self.entries_skipped = 0;
        self.objects_installed = 0;
        self.prototype_objects_installed = 0;
        self.namespace_objects_installed = 0;
        self.native_functions_installed = 0;
        self.strings_interned = 0;
        self.gc_allocations = 0;
        self.gc_allocated_bytes = 0;
        self.duplicate_name_checks = 0;
        self.duplicate_names_found = 0;
        self.phases.clear();
    }

    fn push_phase(&mut self, name: &'static str, duration: Duration) {
        self.phases.push(BootstrapPhaseTelemetry { name, duration });
    }

    fn record_global_this(&mut self) {
        self.objects_installed = self.objects_installed.saturating_add(1);
    }

    fn record_placeholder(&mut self) {
        self.entries_installed = self.entries_installed.saturating_add(1);
        self.objects_installed = self.objects_installed.saturating_add(2);
        self.prototype_objects_installed = self.prototype_objects_installed.saturating_add(1);
    }

    fn record_namespace(&mut self, spec: &NamespaceSpec) {
        self.entries_installed = self.entries_installed.saturating_add(1);
        self.objects_installed = self.objects_installed.saturating_add(1);
        self.namespace_objects_installed = self.namespace_objects_installed.saturating_add(1);
        self.native_functions_installed = self
            .native_functions_installed
            .saturating_add(namespace_native_function_count(spec));
    }

    fn record_skipped_entry(&mut self) {
        self.entries_skipped = self.entries_skipped.saturating_add(1);
    }

    fn finish_allocations(&mut self, before: AllocationSnapshot, after: AllocationSnapshot) {
        self.gc_allocations = after.allocations.saturating_sub(before.allocations);
        self.gc_allocated_bytes = after.live_bytes.saturating_sub(before.live_bytes);
    }
}

#[derive(Debug, Clone, Copy)]
struct AllocationSnapshot {
    allocations: u64,
    live_bytes: usize,
}

/// Deterministic global bootstrap registry.
///
/// Order is significant: every entry whose `install` callback links
/// its prototype to `Object.prototype` (via §19.1.2 / §23.1) must
/// come *after* `Object`. The current layout installs `Object` first
/// so subsequent entries can resolve `globalThis.Object.prototype`
/// without falling through to a null `[[Prototype]]`.
pub static BOOTSTRAP_ENTRIES: &[BootstrapEntry] = &[
    crate::bootstrap_entry!(crate::bootstrap::ObjectIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap::ArrayIntrinsic),
    crate::bootstrap_entry!(crate::json::Intrinsic),
    crate::bootstrap_entry!(crate::string::intrinsic::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap::NumberIntrinsic),
    crate::bootstrap_entry!(crate::boolean::intrinsic::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap_bigint::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap::SymbolIntrinsic),
    crate::bootstrap_entry!(crate::math::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap::DateIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_regexp::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::MapIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::SetIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::WeakMapIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_collections::WeakSetIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_weak_refs::WeakRefIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_promise::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap::ProxyIntrinsic),
    crate::bootstrap_entry!(crate::reflect::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap::FunctionIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_array_buffer::ArrayBufferIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_array_buffer::SharedArrayBufferIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_data_view::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Int8ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint8ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint8ClampedArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Int16ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint16ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Int32ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Uint32ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Float32ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::Float64ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::BigInt64ArrayIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_typed_array::BigUint64ArrayIntrinsic),
    crate::bootstrap_entry!(crate::atomics::Intrinsic),
    crate::bootstrap_entry!(crate::bootstrap::IntlIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap::TemporalIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap::AggregateErrorIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap_weak_refs::FinalizationRegistryIntrinsic),
    crate::bootstrap_entry!(crate::bootstrap::IteratorIntrinsic),
    crate::bootstrap_entry!(crate::console::Intrinsic),
    crate::bootstrap_entry!(crate::timers::Intrinsic),
];

/// Build `globalThis` with all default features.
pub(crate) fn build_global_this(heap: &mut otter_gc::GcHeap) -> Result<JsObject, JsSurfaceError> {
    build_global_this_with_features(heap, BootstrapFeatures::all())
}

/// Build `globalThis` with explicit feature gates.
///
/// This is primarily useful for startup benchmarks and feature-gate tests.
/// The default interpreter path calls [`build_global_this`].
pub fn build_global_this_with_features(
    heap: &mut otter_gc::GcHeap,
    features: BootstrapFeatures,
) -> Result<JsObject, JsSurfaceError> {
    build_global_this_impl(heap, features, None)
}

/// Build `globalThis` while collecting opt-in startup telemetry.
///
/// This entry point exists for Criterion benchmarks and task ratchets. The
/// production runtime path calls [`build_global_this`] and does not maintain
/// telemetry counters.
pub fn build_global_this_with_telemetry(
    heap: &mut otter_gc::GcHeap,
    features: BootstrapFeatures,
    telemetry: &mut BootstrapTelemetry,
) -> Result<JsObject, JsSurfaceError> {
    build_global_this_impl(heap, features, Some(telemetry))
}

fn build_global_this_impl(
    heap: &mut otter_gc::GcHeap,
    features: BootstrapFeatures,
    mut telemetry: Option<&mut BootstrapTelemetry>,
) -> Result<JsObject, JsSurfaceError> {
    let before = telemetry.as_deref_mut().map(|t| {
        t.reset();
        t.entries_considered = BOOTSTRAP_ENTRIES.len();
        let start = Instant::now();
        let duplicates = duplicate_name_count();
        t.duplicate_name_checks = BOOTSTRAP_ENTRIES.len();
        t.duplicate_names_found = duplicates;
        t.push_phase("duplicate-name-validation", start.elapsed());
        allocation_snapshot(heap)
    });

    let global = alloc_object_with_value_roots(heap, &[])?;
    object::set(global, heap, "globalThis", Value::Object(global));
    // §19.1 — `NaN`, `Infinity`, `undefined` are own properties of
    // the global object with writable / enumerable / configurable
    // all false. Reflective lookups (`Object.getOwnPropertyDescriptor(
    // globalThis, "NaN")`) observe the exact attributes.
    object::define_own_property(
        global,
        heap,
        "NaN",
        crate::object::PropertyDescriptor::data(
            Value::Number(crate::number::NumberValue::from_f64(f64::NAN)),
            false,
            false,
            false,
        ),
    );
    object::define_own_property(
        global,
        heap,
        "Infinity",
        crate::object::PropertyDescriptor::data(
            Value::Number(crate::number::NumberValue::from_f64(f64::INFINITY)),
            false,
            false,
            false,
        ),
    );
    object::define_own_property(
        global,
        heap,
        "undefined",
        crate::object::PropertyDescriptor::data(Value::Undefined, false, false, false),
    );
    if let Some(t) = telemetry.as_deref_mut() {
        t.record_global_this();
    }
    for entry in BOOTSTRAP_ENTRIES {
        if features.contains(entry.feature) {
            let start = telemetry.as_ref().map(|_| Instant::now());
            (entry.install)(entry, heap, global)?;
            if let Some(t) = telemetry.as_deref_mut() {
                if let Some(start) = start {
                    t.push_phase(entry.name, start.elapsed());
                }
                record_installed_entry(t, entry);
            }
        } else if let Some(t) = telemetry.as_deref_mut() {
            t.record_skipped_entry();
        }
    }
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(global, heap, Some(object_proto));
    }
    if let (Some(t), Some(before)) = (telemetry, before) {
        let after = allocation_snapshot(heap);
        t.finish_allocations(before, after);
    }
    Ok(global)
}

/// Build a bootstrap entry for one of the 11 concrete TypedArray
/// constructors. Routes to
/// [`crate::bootstrap_typed_array::install_typed_array_entry`].
fn install_placeholder(
    name: &'static str,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let global_root = Value::Object(global);
    let placeholder = alloc_object_with_value_roots(heap, &[&global_root])?;
    let placeholder_root = Value::Object(placeholder);
    let proto = alloc_object_with_value_roots(heap, &[&global_root, &placeholder_root])?;
    object::set(placeholder, heap, "prototype", Value::Object(proto));
    define_global(global, heap, name, Value::Object(placeholder));
    Ok(())
}

fn install_proxy(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCtx, NativeError};

    fn proxy_target_is_object(value: &Value) -> bool {
        matches!(
            value,
            Value::Object(_)
                | Value::Array(_)
                | Value::Function { .. }
                | Value::Closure(_)
                | Value::NativeFunction(_)
                | Value::BoundFunction(_)
                | Value::ClassConstructor(_)
                | Value::Promise(_)
                | Value::Iterator(_)
                | Value::RegExp(_)
                | Value::Map(_)
                | Value::Set(_)
                | Value::WeakMap(_)
                | Value::WeakSet(_)
                | Value::WeakRef(_)
                | Value::FinalizationRegistry(_)
                | Value::Temporal(_)
                | Value::Intl(_)
                | Value::ArrayBuffer(_)
                | Value::DataView(_)
                | Value::TypedArray(_)
                | Value::Generator(_)
                | Value::Proxy(_)
        )
    }

    fn proxy_target_arg(args: &[Value]) -> Result<Value, NativeError> {
        match args.first() {
            Some(value) if proxy_target_is_object(value) => Ok(value.clone()),
            _ => Err(NativeError::TypeError {
                name: "Proxy",
                reason: "target must be an object".to_string(),
            }),
        }
    }

    fn proxy_handler_arg(args: &[Value]) -> Result<Value, NativeError> {
        match args.get(1) {
            Some(value) if proxy_target_is_object(value) => Ok(value.clone()),
            _ => Err(NativeError::TypeError {
                name: "Proxy",
                reason: "handler must be an object".to_string(),
            }),
        }
    }

    fn proxy_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        if !ctx.is_construct_call() {
            return Err(NativeError::TypeError {
                name: "Proxy",
                reason: "constructor requires new".to_string(),
            });
        }
        let target = proxy_target_arg(args)?;
        let handler = proxy_handler_arg(args)?;
        let proxy = crate::proxy::JsProxy::new(ctx.heap_mut(), target, handler).map_err(|_| {
            NativeError::TypeError {
                name: "Proxy",
                reason: "out of memory while allocating proxy".to_string(),
            }
        })?;
        Ok(Value::Proxy(proxy))
    }

    fn proxy_revocable_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let target = proxy_target_arg(args)?;
        let handler = proxy_handler_arg(args)?;
        let proxy = crate::proxy::JsProxy::new(ctx.heap_mut(), target, handler).map_err(|_| {
            NativeError::TypeError {
                name: "Proxy.revocable",
                reason: "out of memory while allocating proxy".to_string(),
            }
        })?;
        let proxy_value = Value::Proxy(proxy);
        let revoke = ctx
            .native_value_with_captures(
                "revoke",
                smallvec::smallvec![proxy_value.clone()],
                &[],
                &[args],
                move |ctx, _, captures| {
                    if let Some(Value::Proxy(proxy)) = captures.first() {
                        proxy.revoke(ctx.heap_mut());
                    }
                    Ok(Value::Undefined)
                },
            )
            .map_err(|_| NativeError::TypeError {
                name: "Proxy.revocable",
                reason: "out of memory while creating revoke function".to_string(),
            })?;
        let obj = ctx
            .alloc_object_with_roots(&[&proxy_value, &revoke], &[args])
            .map_err(|_| NativeError::TypeError {
                name: "Proxy.revocable",
                reason: "out of memory while creating result object".to_string(),
            })?;
        object::set(obj, ctx.heap_mut(), "proxy", proxy_value);
        object::set(obj, ctx.heap_mut(), "revoke", revoke);
        Ok(Value::Object(obj))
    }

    let global_root = Value::Object(global);
    let proxy_ctor = native_constructor_static_with_value_roots(
        heap,
        "Proxy",
        2,
        proxy_ctor_call,
        &[&global_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let proxy_ctor_root = Value::NativeFunction(proxy_ctor);
    let revocable = native_static_with_value_roots(
        heap,
        "revocable",
        2,
        proxy_revocable_call,
        &[&global_root, &proxy_ctor_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let revocable_desc =
        PropertyDescriptor::data(Value::NativeFunction(revocable), true, false, true);
    let string_heap = crate::string::StringHeap::default();
    if !proxy_ctor.define_own_property(heap, &string_heap, "revocable", revocable_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("revocable"));
    }
    define_global(global, heap, "Proxy", Value::NativeFunction(proxy_ctor));
    Ok(())
}

// §20.4.1 The Symbol Constructor — ordinary function callable as
// `Symbol(desc)`. Calling with `new` rejects per §20.4.1.1.
// Exposes every well-known symbol as an own data property
// (configurable=false, writable=false, enumerable=false per
// §20.4.2.*), plus `for` / `keyFor` methods and a `prototype` link.
// <https://tc39.es/ecma262/#sec-symbol-constructor>
fn install_symbol(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;
    use crate::{NativeCtx, NativeError};

    fn symbol_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        if ctx.is_construct_call() {
            return Err(NativeError::TypeError {
                name: "Symbol",
                reason: "Symbol is not a constructor".to_string(),
            });
        }
        let description =
            match args.first() {
                None | Some(Value::Undefined) => None,
                Some(other) => {
                    let context =
                        ctx.execution_context()
                            .cloned()
                            .ok_or_else(|| NativeError::TypeError {
                                name: "Symbol",
                                reason: "missing execution context".to_string(),
                            })?;
                    let coerced = ctx
                        .cx
                        .interp
                        .coerce_to_string(&context, other)
                        .map_err(|e| match e {
                            crate::VmError::TypeError { message } => NativeError::TypeError {
                                name: "Symbol",
                                reason: message,
                            },
                            crate::VmError::Uncaught { value } => NativeError::Thrown {
                                name: "Symbol",
                                message: value,
                            },
                            other => NativeError::TypeError {
                                name: "Symbol",
                                reason: other.to_string(),
                            },
                        })?;
                    let string_heap = ctx.interp_mut().string_heap_clone();
                    let rendered = crate::string::JsString::from_str(&coerced, &string_heap)
                        .map_err(|_| NativeError::TypeError {
                            name: "Symbol",
                            reason: "out of memory".to_string(),
                        })?;
                    Some(rendered)
                }
            };
        let sym = crate::symbol::JsSymbol::new(ctx.interp_mut().gc_heap_mut(), description)
            .map_err(|_| NativeError::TypeError {
                name: "Symbol",
                reason: "out of memory".to_string(),
            })?;
        Ok(Value::Symbol(sym))
    }

    fn symbol_for_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let key = match args.first() {
            None | Some(Value::Undefined) => "undefined".to_string(),
            Some(other) => {
                let context =
                    ctx.execution_context()
                        .cloned()
                        .ok_or_else(|| NativeError::TypeError {
                            name: "Symbol.for",
                            reason: "missing execution context".to_string(),
                        })?;
                ctx.cx
                    .interp
                    .coerce_to_string(&context, other)
                    .map_err(|e| match e {
                        crate::VmError::TypeError { message } => NativeError::TypeError {
                            name: "Symbol.for",
                            reason: message,
                        },
                        crate::VmError::Uncaught { value } => NativeError::Thrown {
                            name: "Symbol.for",
                            message: value,
                        },
                        other => NativeError::TypeError {
                            name: "Symbol.for",
                            reason: other.to_string(),
                        },
                    })?
            }
        };
        let sym = ctx
            .interp_mut()
            .symbol_for_key(&key)
            .map_err(|_| NativeError::TypeError {
                name: "Symbol.for",
                reason: "out of memory".to_string(),
            })?;
        Ok(Value::Symbol(sym))
    }

    fn symbol_key_for_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let Some(Value::Symbol(sym)) = args.first() else {
            return Err(NativeError::TypeError {
                name: "Symbol.keyFor",
                reason: "argument must be a symbol".to_string(),
            });
        };
        let key = ctx.interp_mut().symbol_registry().key_for(sym);
        match key {
            Some(key) => {
                let string_heap = ctx.interp_mut().string_heap_clone();
                let value =
                    crate::string::JsString::from_str(&key, &string_heap).map_err(|_| {
                        NativeError::TypeError {
                            name: "Symbol.keyFor",
                            reason: "out of memory".to_string(),
                        }
                    })?;
                Ok(Value::String(value))
            }
            None => Ok(Value::Undefined),
        }
    }

    fn symbol_proto_to_string(
        ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        let this = ctx.this_value().clone();
        let sym = match &this {
            Value::Symbol(sym) => sym.clone(),
            Value::Object(obj) => {
                let heap = ctx.interp_mut().gc_heap();
                crate::object::symbol_data(*obj, heap).ok_or_else(|| NativeError::TypeError {
                    name: "Symbol.prototype.toString",
                    reason: "this is not a Symbol".to_string(),
                })?
            }
            _ => {
                return Err(NativeError::TypeError {
                    name: "Symbol.prototype.toString",
                    reason: "this is not a Symbol".to_string(),
                });
            }
        };
        let string_heap = ctx.interp_mut().string_heap_clone();
        let s = crate::string::JsString::from_str(&sym.descriptive_string(), &string_heap)
            .map_err(|_| NativeError::TypeError {
                name: "Symbol.prototype.toString",
                reason: "out of memory".to_string(),
            })?;
        Ok(Value::String(s))
    }

    fn symbol_proto_value_of(
        ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        match ctx.this_value().clone() {
            Value::Symbol(sym) => Ok(Value::Symbol(sym.clone())),
            Value::Object(obj) => {
                let heap = ctx.interp_mut().gc_heap();
                crate::object::symbol_data(obj, heap)
                    .map(Value::Symbol)
                    .ok_or_else(|| NativeError::TypeError {
                        name: "Symbol.prototype.valueOf",
                        reason: "this is not a Symbol".to_string(),
                    })
            }
            _ => Err(NativeError::TypeError {
                name: "Symbol.prototype.valueOf",
                reason: "this is not a Symbol".to_string(),
            }),
        }
    }

    fn symbol_proto_to_primitive(
        ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        match ctx.this_value().clone() {
            Value::Symbol(sym) => Ok(Value::Symbol(sym)),
            Value::Object(obj) => {
                let heap = ctx.interp_mut().gc_heap();
                crate::object::symbol_data(obj, heap)
                    .map(Value::Symbol)
                    .ok_or_else(|| NativeError::TypeError {
                        name: "Symbol.prototype[@@toPrimitive]",
                        reason: "this is not a Symbol".to_string(),
                    })
            }
            _ => Err(NativeError::TypeError {
                name: "Symbol.prototype[@@toPrimitive]",
                reason: "this is not a Symbol".to_string(),
            }),
        }
    }

    // The Symbol constructor itself is a callable NativeFunction.
    let global_root = Value::Object(global);
    let symbol_ctor = {
        let mut external_visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            global_root.trace_value_slots(visitor);
        };
        crate::native_function::NativeFunction::new_constructor_static_with_roots(
            heap,
            "Symbol",
            0,
            symbol_ctor_call,
            &mut external_visit,
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?
    };

    // §20.4.3 Symbol.prototype — ordinary object linked to %Object.prototype%.
    let symbol_ctor_root = Value::NativeFunction(symbol_ctor);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &symbol_ctor_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    fn symbol_proto_description_get(
        ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        match ctx.this_value().clone() {
            Value::Symbol(sym) => match sym.description() {
                Some(s) => Ok(Value::String(s.clone())),
                None => Ok(Value::Undefined),
            },
            Value::Object(obj) => {
                let heap = ctx.interp_mut().gc_heap();
                match crate::object::symbol_data(obj, heap) {
                    Some(sym) => match sym.description() {
                        Some(s) => Ok(Value::String(s.clone())),
                        None => Ok(Value::Undefined),
                    },
                    None => Err(NativeError::TypeError {
                        name: "get Symbol.prototype.description",
                        reason: "this is not a Symbol".to_string(),
                    }),
                }
            }
            _ => Err(NativeError::TypeError {
                name: "get Symbol.prototype.description",
                reason: "this is not a Symbol".to_string(),
            }),
        }
    }

    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root.clone(), symbol_ctor_root.clone()],
        );
        builder.method(
            "toString",
            0,
            crate::native_function::NativeCall::Static(symbol_proto_to_string),
            Attr::builtin_function(),
        )?;
        builder.method(
            "valueOf",
            0,
            crate::native_function::NativeCall::Static(symbol_proto_value_of),
            Attr::builtin_function(),
        )?;
        // §20.4.3.2 Symbol.prototype.description — accessor.
        let prototype_root = Value::Object(prototype);
        let getter = native_static_with_value_roots(
            heap,
            "get description",
            0,
            symbol_proto_description_get,
            &[&global_root, &symbol_ctor_root, &prototype_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let desc_desc =
            PropertyDescriptor::accessor(Some(Value::NativeFunction(getter)), None, false, true);
        if !object::define_own_property(prototype, heap, "description", desc_desc) {
            return Err(JsSurfaceError::DefinePropertyFailed("description"));
        }
    }
    // Install Symbol.prototype as an own property on the constructor.
    let proto_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    let string_heap = crate::string::StringHeap::default();
    if !symbol_ctor.define_own_property(heap, &string_heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    // Well-known symbol own properties (`Symbol.iterator`,
    // `Symbol.toPrimitive`, …) are installed by
    // [`install_symbol_well_knowns_post_bootstrap`] once the
    // per-interpreter `WellKnownSymbols` singleton table exists.
    // `for` / `keyFor` methods.
    let prototype_root = Value::Object(prototype);
    let symbol_for_fn = native_static_with_value_roots(
        heap,
        "for",
        1,
        symbol_for_call,
        &[&global_root, &symbol_ctor_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let symbol_for_root = Value::NativeFunction(symbol_for_fn);
    let symbol_key_for_fn = native_static_with_value_roots(
        heap,
        "keyFor",
        1,
        symbol_key_for_call,
        &[
            &global_root,
            &symbol_ctor_root,
            &prototype_root,
            &symbol_for_root,
        ],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let for_desc =
        PropertyDescriptor::data(Value::NativeFunction(symbol_for_fn), true, false, true);
    let key_for_desc =
        PropertyDescriptor::data(Value::NativeFunction(symbol_key_for_fn), true, false, true);
    if !symbol_ctor.define_own_property(heap, &string_heap, "for", for_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("for"));
    }
    if !symbol_ctor.define_own_property(heap, &string_heap, "keyFor", key_for_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("keyFor"));
    }
    // Install Symbol.prototype.constructor → Symbol.
    let constructor_desc =
        PropertyDescriptor::data(Value::NativeFunction(symbol_ctor), true, false, true);
    if !object::define_own_property(prototype, heap, "constructor", constructor_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("constructor"));
    }
    // Symbol.prototype[@@toPrimitive] is installed by
    // `install_symbol_well_knowns_post_bootstrap` so it points at
    // the per-realm well-known JsSymbol singleton.
    let _ = WellKnown::Iterator; // silence the unused-import lint
    let _ = symbol_proto_to_primitive;
    define_global(global, heap, "Symbol", Value::NativeFunction(symbol_ctor));
    Ok(())
}

/// Post-bootstrap fixup: install every well-known symbol as an own
/// property on the realm's `Symbol` constructor plus
/// `Symbol.prototype[@@toPrimitive]`. Bootstrap runs before the
/// per-interpreter [`crate::WellKnownSymbols`] table exists, so the
/// runtime calls this hook from `Interpreter::with_string_heap_cap`
/// once the table is materialised.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-symbol.iterator>
/// - <https://tc39.es/ecma262/#sec-symbol.prototype-@@toprimitive>
pub fn install_symbol_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    fn symbol_proto_to_primitive(
        ctx: &mut crate::NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, crate::NativeError> {
        match ctx.this_value().clone() {
            Value::Symbol(sym) => Ok(Value::Symbol(sym)),
            Value::Object(obj) => {
                let heap = ctx.interp_mut().gc_heap();
                crate::object::symbol_data(obj, heap)
                    .map(Value::Symbol)
                    .ok_or_else(|| crate::NativeError::TypeError {
                        name: "Symbol.prototype[@@toPrimitive]",
                        reason: "this is not a Symbol".to_string(),
                    })
            }
            _ => Err(crate::NativeError::TypeError {
                name: "Symbol.prototype[@@toPrimitive]",
                reason: "this is not a Symbol".to_string(),
            }),
        }
    }

    let Some(symbol_ctor_value) = object::get(global, heap, "Symbol") else {
        return Ok(());
    };
    let symbol_ctor = match symbol_ctor_value {
        Value::NativeFunction(f) => f,
        _ => return Ok(()),
    };

    let well_known_pairs: &[(&'static str, WellKnown)] = &[
        ("asyncIterator", WellKnown::AsyncIterator),
        ("hasInstance", WellKnown::HasInstance),
        ("isConcatSpreadable", WellKnown::IsConcatSpreadable),
        ("iterator", WellKnown::Iterator),
        ("match", WellKnown::Match),
        ("matchAll", WellKnown::MatchAll),
        ("replace", WellKnown::Replace),
        ("search", WellKnown::Search),
        ("species", WellKnown::Species),
        ("split", WellKnown::Split),
        ("toPrimitive", WellKnown::ToPrimitive),
        ("toStringTag", WellKnown::ToStringTag),
        ("unscopables", WellKnown::Unscopables),
    ];
    for (name, tag) in well_known_pairs {
        let sym = well_known.get(*tag);
        let desc = PropertyDescriptor::data(Value::Symbol(sym), false, false, false);
        if !symbol_ctor.define_own_property(heap, string_heap, name, desc) {
            return Err(JsSurfaceError::DefinePropertyFailed("well-known symbol"));
        }
    }

    // Symbol.prototype[@@toPrimitive] — ECMA-262 §20.4.3.5.
    let proto_desc = symbol_ctor
        .own_property_descriptor(heap, string_heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match proto_desc.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data {
            value: Value::Object(p),
        } => Some(p),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let symbol_ctor_root = Value::NativeFunction(symbol_ctor);
    let prototype_root = Value::Object(prototype);
    let to_prim_fn = native_static_with_value_roots(
        heap,
        "[Symbol.toPrimitive]",
        1,
        symbol_proto_to_primitive,
        &[&symbol_ctor_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let to_primitive_sym = well_known.get(WellKnown::ToPrimitive);
    let to_prim_desc =
        PropertyDescriptor::data(Value::NativeFunction(to_prim_fn), false, false, true);
    if !object::define_own_symbol_property(prototype, heap, &to_primitive_sym, to_prim_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed(
            "Symbol.prototype[@@toPrimitive]",
        ));
    }

    // §22.2 / §25.1 / §25.4 — install `@@toStringTag` on standard
    // namespace objects so `Object.prototype.toString.call(NS)`
    // returns the spec-required `"[object <NS>]"` form. Also wire
    // their `[[Prototype]]` to `%Object.prototype%` per §21.3.1 /
    // §25.5.1 / §28.1 so inherited reads (`Math.hasOwnProperty`,
    // `Object.prototype.value` shadowing during `ToPropertyDescriptor`)
    // resolve correctly.
    let to_string_tag_sym = well_known.get(WellKnown::ToStringTag);
    let object_proto = object::get(global, heap, "Object").and_then(|v| match v {
        Value::NativeFunction(ctor) => ctor
            .own_property_descriptor(heap, string_heap, "prototype")
            .ok()
            .flatten()
            .and_then(|d| match d.kind {
                crate::object::DescriptorKind::Data {
                    value: Value::Object(p),
                } => Some(p),
                _ => None,
            }),
        Value::Object(ctor) => match object::get(ctor, heap, "prototype") {
            Some(Value::Object(p)) => Some(p),
            _ => None,
        },
        _ => None,
    });
    for ns_name in ["Math", "JSON", "Reflect", "Atomics"] {
        if let Some(Value::Object(ns)) = object::get(global, heap, ns_name) {
            if let Some(proto) = object_proto {
                object::set_prototype(ns, heap, Some(proto));
            }
            let tag = crate::string::JsString::from_str(ns_name, string_heap)
                .map_err(|_| JsSurfaceError::OutOfMemory)?;
            object::define_own_symbol_property_partial(
                ns,
                heap,
                &to_string_tag_sym,
                crate::object::PartialPropertyDescriptor {
                    value: Some(Value::String(tag)),
                    writable: Some(false),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );
        }
    }
    // §20.4.3.5 — install `Symbol.prototype[@@toStringTag] = "Symbol"`.
    let symbol_tag = crate::string::JsString::from_str("Symbol", string_heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &to_string_tag_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::String(symbol_tag)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    // §24.* — install collection `@@iterator` / `@@toStringTag`.
    crate::bootstrap_collections::install_collection_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §27.2.5.5 — install `Promise.prototype[@@toStringTag]`.
    crate::bootstrap_promise::install_promise_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §26.1.4.4 / §26.2.4.5 — `WeakRef.prototype[@@toStringTag]`
    // + `FinalizationRegistry.prototype[@@toStringTag]`.
    crate::bootstrap_weak_refs::install_weak_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §21.2.5 — `BigInt.prototype[@@toStringTag]`.
    crate::bootstrap_bigint::install_bigint_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §25.3.5 — `DataView.prototype[@@toStringTag]`.
    crate::bootstrap_data_view::install_data_view_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §23.2.4 — `%TypedArray%.prototype[@@iterator]` plus per-kind
    // `<T>.prototype[@@toStringTag]`.
    crate::bootstrap_typed_array::install_typed_array_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    // §27.1.2 — `Iterator.prototype[@@iterator]` (returns this) and
    // `[@@toStringTag] = "Iterator"`.
    install_iterator_well_knowns_post_bootstrap(heap, string_heap, global, well_known)?;
    // §25.1.5 — `ArrayBuffer.prototype[@@toStringTag]`.
    crate::bootstrap_array_buffer::install_array_buffer_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §21.4.4.45 — `Date.prototype[@@toPrimitive]`.
    crate::date::well_known::install_date_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §22.1.3.34 — `String.prototype[@@iterator]`.
    crate::install_string_iterator_post_bootstrap(heap, global, well_known)?;
    // §22.2.6.{8,10} — `RegExp.prototype[@@match]` / `[@@search]`.
    crate::bootstrap_regexp::install_regexp_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // §25.2.5 — `SharedArrayBuffer.prototype[@@toStringTag]`.
    crate::bootstrap_array_buffer::install_shared_array_buffer_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    // Default `get <Ctor>[@@species]` returning `this` for every
    // subclassing-aware constructor that the spec lists in the
    // species table. Each accessor body is identical (§7.3.21:
    // SpeciesConstructor consults this slot when present).
    //
    // - Array      §23.1.2.4 https://tc39.es/ecma262/#sec-get-array-@@species
    // - Map        §24.1.2.1 https://tc39.es/ecma262/#sec-get-map-@@species
    // - Set        §24.2.2.1 https://tc39.es/ecma262/#sec-get-set-@@species
    // - RegExp     §22.2.5.1 https://tc39.es/ecma262/#sec-get-regexp-@@species
    // - ArrayBuffer       §25.1.5.3 https://tc39.es/ecma262/#sec-get-arraybuffer-@@species
    // - SharedArrayBuffer §25.2.4.2 https://tc39.es/ecma262/#sec-sharedarraybuffer-@@species
    for ctor_name in [
        "Array",
        "Map",
        "Set",
        "RegExp",
        "ArrayBuffer",
        "SharedArrayBuffer",
    ] {
        install_constructor_species_accessor(heap, string_heap, global, well_known, ctor_name)?;
    }
    Ok(())
}

/// Install the default `get <Ctor>[@@species]` accessor — returns the
/// `this` value, configurable, non-enumerable. Used by every
/// subclassing-aware builtin per §22.1.2.5 (Array), §24.1.2.1 (Map),
/// §24.2.2.1 (Set), §22.2.5.1 (RegExp), §25.1.5.3 (ArrayBuffer),
/// §25.2.4.2 (SharedArrayBuffer).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-symbol.species>
fn install_constructor_species_accessor(
    heap: &mut otter_gc::GcHeap,
    _string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
    ctor_name: &'static str,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    fn species_get(
        ctx: &mut crate::NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, crate::NativeError> {
        Ok(ctx.this_value().clone())
    }

    let Some(ctor_value) = object::get(global, heap, ctor_name) else {
        return Ok(());
    };
    let global_root = Value::Object(global);
    let ctor_root = ctor_value.clone();
    let species_getter = native_static_with_value_roots(
        heap,
        "get [Symbol.species]",
        0,
        species_get,
        &[&global_root, &ctor_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let species_sym = well_known.get(WellKnown::Species);
    let installed = match ctor_value {
        Value::NativeFunction(f) => f.define_own_symbol_property(
            heap,
            &species_sym,
            crate::object::PartialPropertyDescriptor {
                get: Some(Value::NativeFunction(species_getter)),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        ),
        Value::Object(obj) => {
            crate::object::define_own_symbol_property_partial(
                obj,
                heap,
                &species_sym,
                crate::object::PartialPropertyDescriptor {
                    get: Some(Value::NativeFunction(species_getter)),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );
            true
        }
        _ => return Ok(()),
    };
    if !installed {
        return Err(JsSurfaceError::DefinePropertyFailed(
            "constructor[Symbol.species]",
        ));
    }
    Ok(())
}

fn install_array(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCtx, NativeError};

    let global_root = Value::Object(global);
    let array = alloc_object_with_value_roots(heap, &[&global_root])?;
    let array_root = Value::Object(array);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &array_root])?;
    // §23.1 — `Array.prototype` is itself an Array exotic object whose
    // `[[Prototype]]` is `%Object.prototype%`. Bootstrap order installs
    // `Object` first, so the realm's Object.prototype is reachable at
    // this point. Linking the chain here keeps the §7.1.1 / §7.1.1.1
    // `ToPrimitive` / `OrdinaryToPrimitive` lookup path working for
    // `Value::Array` operands — without it, `[1,2,3] + ""` walks an
    // empty proto chain and reaches the foundation TypeError ladder.
    // <https://tc39.es/ecma262/#sec-properties-of-the-array-prototype-object>
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(array, heap, Some(object_proto));
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    let _ = object::define_own_property(
        array,
        heap,
        "prototype",
        crate::object::PropertyDescriptor::data(Value::Object(prototype), false, false, false),
    );
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            array,
            vec![global_root.clone(), Value::Object(prototype)],
        );
        for method in array_statics::ARRAY_STATIC_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root.clone(), array_root.clone()],
        );
        for method in array_prototype::ARRAY_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }

    // §23.1.1.1 Array(...values) — both `Array(…)` and
    // `new Array(…)` reach this callback. Single numeric argument
    // means "pre-sized sparse array of length n"; anything else
    // collects values verbatim.
    // <https://tc39.es/ecma262/#sec-array>
    fn array_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        if !(args.len() == 1 && matches!(args.first(), Some(Value::Number(_)))) {
            let arr = ctx.array_from_elements(args.iter().cloned()).map_err(|_| {
                NativeError::TypeError {
                    name: "Array",
                    reason: "out of memory while allocating array".to_string(),
                }
            })?;
            apply_array_new_target_prototype(ctx, arr)?;
            return Ok(Value::Array(arr));
        }
        let arr =
            ctx.array_from_elements(std::iter::empty())
                .map_err(|_| NativeError::TypeError {
                    name: "Array",
                    reason: "out of memory while allocating array".to_string(),
                })?;
        if let Value::Number(n) = &args[0] {
            let raw = n.as_f64();
            let len = raw as u32;
            if !raw.is_finite() || raw < 0.0 || raw != f64::from(len) {
                return Err(NativeError::RangeError {
                    name: "Array",
                    reason: "Invalid array length".to_string(),
                });
            }
            if len > 0 {
                // `array::set` gap-fills with `Value::Hole`, so
                // writing the trailing slot also fills every index
                // in `[0, len-1)` with a hole.
                let last = (len - 1) as usize;
                ctx.array_set(arr, last, Value::Hole)
                    .map_err(|_| NativeError::TypeError {
                        name: "Array",
                        reason: "out of memory while sizing array".to_string(),
                    })?;
            }
            apply_array_new_target_prototype(ctx, arr)?;
            return Ok(Value::Array(arr));
        }
        unreachable!("non-numeric Array(...) arguments returned above")
    }

    fn apply_array_new_target_prototype(
        ctx: &mut NativeCtx<'_>,
        arr: array::JsArray,
    ) -> Result<(), NativeError> {
        let Some(new_target) = ctx.new_target().cloned() else {
            return Ok(());
        };
        let proto = match new_target {
            Value::ClassConstructor(class) => Some(Value::Object(class.prototype(ctx.heap()))),
            Value::Object(obj) => object::get(obj, ctx.heap(), "prototype").filter(|value| {
                constructor_return_is_object(value) || matches!(value, Value::Proxy(_))
            }),
            Value::NativeFunction(native) => native
                .own_property_descriptor(ctx.heap(), ctx.cx.interp.string_heap(), "prototype")
                .map_err(|err| NativeError::TypeError {
                    name: "Array",
                    reason: err.to_string(),
                })?
                .map(|descriptor| descriptor_value(&descriptor))
                .filter(|value| {
                    constructor_return_is_object(value) || matches!(value, Value::Proxy(_))
                }),
            _ => None,
        };
        if let Some(proto) = proto {
            array::set_prototype_override(arr, ctx.heap_mut(), Some(proto));
        }
        Ok(())
    }

    let ctor_native = native_static_with_value_roots(
        heap,
        "Array",
        1,
        array_ctor_call,
        &[&global_root, &array_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    // Wire the callable+constructable bridge as an internal object
    // slot. This must not appear in JS own-property reflection.
    object::set_constructor_native(array, heap, Value::NativeFunction(ctor_native));

    // §23.1.3.1 — `Array.prototype.constructor = Array`, writable,
    // non-enumerable, configurable.
    object::define_own_property(
        prototype,
        heap,
        "constructor",
        crate::object::PropertyDescriptor::data(Value::Object(array), true, false, true),
    );

    define_global(global, heap, "Array", Value::Object(array));
    Ok(())
}

fn install_number(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCall, NativeCtx, NativeError};

    let global_root = Value::Object(global);
    // Number.prototype with all the formatter methods + the
    // hidden `[[NumberData]]` slot (= +0 per §21.1.3) so
    // `Number.prototype.toString()` recovers the value.
    let prototype = alloc_object_with_value_roots(heap, &[&global_root])?;
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root.clone()]);
        for method in crate::number::prototype::NUMBER_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    crate::object::set_number_data(prototype, heap, crate::number::NumberValue::from_i32(0));

    // §21.1.1 Number constructor. Both `Number(value)` (call) and
    // `new Number(value)` (construct) coerce `value` via §7.1.4
    // ToNumber. The construct form additionally wraps the result in
    // a `NumberObject` with `[[NumberData]] = ToNumber(value)`; the
    // pre-allocated receiver from `dispatch_construct` already has
    // `Number.prototype` linked as `[[Prototype]]`.
    fn number_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let value = if args.is_empty() {
            crate::number::NumberValue::from_i32(0)
        } else {
            // §21.1.1.1 — the `Number(value)` constructor diverges
            // from §7.1.4 ToNumber on BigInt (converts to f64 instead
            // of throwing). Delegate to the dedicated helper so the
            // ToPrimitive ladder, the Symbol guard, and the BigInt
            // path live in one place.
            let context =
                ctx.execution_context()
                    .cloned()
                    .ok_or_else(|| NativeError::TypeError {
                        name: "Number",
                        reason: "missing execution context".to_string(),
                    })?;
            ctx.cx
                .interp
                .number_for_number_ctor(&context, &args[0])
                .map_err(|e| match e {
                    crate::VmError::TypeError { message } => NativeError::TypeError {
                        name: "Number",
                        reason: message,
                    },
                    crate::VmError::Uncaught { value } => NativeError::Thrown {
                        name: "Number",
                        message: value,
                    },
                    other => NativeError::TypeError {
                        name: "Number",
                        reason: other.to_string(),
                    },
                })?
        };
        if ctx.is_construct_call() {
            let this = ctx.this_value().clone();
            if let Value::Object(obj) = this {
                crate::object::set_number_data(obj, ctx.heap_mut(), value);
                Ok(Value::Object(obj))
            } else {
                Err(NativeError::TypeError {
                    name: "Number",
                    reason: "expected object receiver in `new Number(...)`".to_string(),
                })
            }
        } else {
            Ok(Value::Number(value))
        }
    }

    let prototype_root = Value::Object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "Number",
        1,
        number_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_native_root = Value::NativeFunction(ctor_native);
    // The `Number` global itself is a GC-managed JsObject. Both the
    // constants/static methods and the `prototype` link sit on it
    // as ordinary properties; the callable+constructable surface is
    // wired through the dispatch path's internal native-constructor
    // slot.
    let statics =
        alloc_object_with_value_roots(heap, &[&global_root, &prototype_root, &ctor_native_root])?;
    // Chain `Number`'s statics to `Object.prototype` so the
    // prototype-resident methods (hasOwnProperty, toString,
    // isPrototypeOf, etc.) resolve through ordinary property
    // lookup. Object is installed earlier in BOOTSTRAP_ENTRIES, so
    // `Object.prototype` is already reachable.
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(statics, heap, Some(object_proto));
    }
    // Same chaining for `Number.prototype`, so
    // `Number.prototype.hasOwnProperty(...)` resolves.
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    // Wire the callable+constructable bridge: stash the native
    // ctor on the Number object under a reserved key the dispatch
    // path looks up before falling back to ordinary property load.
    object::set_constructor_native(statics, heap, ctor_native_root);
    // `Number.prototype` lives as an own property on the
    // §21.1.2.5 — `Number.prototype` is a non-writable, non-enumerable,
    // non-configurable data property.
    let _ = object::define_own_property(
        statics,
        heap,
        "prototype",
        crate::object::PropertyDescriptor::data(Value::Object(prototype), false, false, false),
    );
    // §21.1.2 — `Number.length` is a non-writable, non-enumerable,
    // configurable data property whose value matches the formal
    // parameter count of the constructor (1).
    let _ = object::define_own_property(
        statics,
        heap,
        "length",
        crate::object::PropertyDescriptor::data(
            Value::Number(crate::number::NumberValue::from_i32(1)),
            false,
            false,
            true,
        ),
    );
    // §21.1.2 — `Number.name` is `"Number"`, non-writable,
    // non-enumerable, configurable.
    let number_name_value = Value::String(
        crate::string::JsString::from_str("Number", &crate::string::StringHeap::default())
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let _ = object::define_own_property(
        statics,
        heap,
        "name",
        crate::object::PropertyDescriptor::data(number_name_value, false, false, true),
    );

    // §21.1.2 Number-namespace constants. Per spec, each is
    // `[[Writable]]: false, [[Enumerable]]: false,
    // [[Configurable]]: false` — install via `Attr::read_only()`
    // through the property builder so descriptor checks pass.
    let max_safe_int = ((1u64 << 53) - 1) as f64;
    let constants: &[(&'static str, f64)] = &[
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("EPSILON", f64::EPSILON),
        ("MAX_SAFE_INTEGER", max_safe_int),
        ("MIN_SAFE_INTEGER", -max_safe_int),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ];
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            statics,
            vec![global_root.clone(), prototype_root.clone()],
        );
        for (name, value) in constants {
            builder.property(
                name,
                Value::Number(crate::number::NumberValue::from_f64(*value)),
                Attr::read_only(),
            )?;
        }
    }

    // Static predicates / parsers. Wired through dedicated native
    // callbacks that share the foundation `crate::number::parse`
    // implementation (the same helpers `Op::GlobalCall` reaches via
    // the compile-time alias for `Number.isNaN(x)` etc.).
    fn number_is_nan_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let result = matches!(args.first(), Some(Value::Number(n)) if n.as_f64().is_nan());
        Ok(Value::Boolean(result))
    }
    fn number_is_finite_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let result = matches!(args.first(), Some(Value::Number(n)) if n.as_f64().is_finite());
        Ok(Value::Boolean(result))
    }
    fn number_is_integer_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let v = args.first().cloned().unwrap_or(Value::Undefined);
        Ok(Value::Boolean(crate::number::parse::is_integer(&v)))
    }
    fn number_is_safe_integer_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let v = args.first().cloned().unwrap_or(Value::Undefined);
        Ok(Value::Boolean(crate::number::parse::is_safe_integer(&v)))
    }
    fn number_parse_int_native(
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = match args.first() {
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(other) => other.display_string(ctx.heap()),
            None => {
                return Ok(Value::Number(crate::number::NumberValue::from_f64(
                    f64::NAN,
                )));
            }
        };
        let radix = match args.get(1) {
            Some(Value::Number(n)) => n.as_f64() as i32,
            _ => 0,
        };
        Ok(Value::Number(crate::number::parse::parse_int(&s, radix)))
    }
    fn number_parse_float_native(
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = match args.first() {
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(other) => other.display_string(ctx.heap()),
            None => {
                return Ok(Value::Number(crate::number::NumberValue::from_f64(
                    f64::NAN,
                )));
            }
        };
        Ok(Value::Number(crate::number::parse::parse_float(&s)))
    }

    {
        let global_root2 = Value::Object(global);
        let statics_root = Value::Object(statics);
        let prototype_root2 = Value::Object(prototype);
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            statics,
            vec![global_root2.clone(), prototype_root2.clone()],
        );
        let methods: &[(&'static str, u8, crate::native_function::NativeFastFn)] = &[
            ("isNaN", 1, number_is_nan_native),
            ("isFinite", 1, number_is_finite_native),
            ("isInteger", 1, number_is_integer_native),
            ("isSafeInteger", 1, number_is_safe_integer_native),
            ("parseInt", 2, number_parse_int_native),
            ("parseFloat", 1, number_parse_float_native),
        ];
        for (name, length, call) in methods {
            builder.method(
                name,
                *length,
                NativeCall::Static(*call),
                Attr::builtin_function(),
            )?;
        }
        // §19.2 — the global `parseInt` / `parseFloat` / `isNaN` /
        // `isFinite` properties are spec-defined to be the **same
        // callable** as their `Number.*` counterparts. Install
        // global aliases now that the Number statics exist. Note
        // these are independent property records pointing at fresh
        // NativeFunction values, not literal slot sharing — the
        // callables match by behaviour, which is what user code
        // observes.
        //
        // The four URI globals (`encodeURI` / `decodeURI` /
        // `encodeURIComponent` / `decodeURIComponent`) install
        // alongside because they share the same prerequisite plumbing
        // and route through the existing `global_functions::call`
        // dispatcher when the compiler emits `Op::GlobalCall` — these
        // natives are only consulted for reflective / `.call` reads.
        fn global_encode_uri(
            ctx: &mut NativeCtx<'_>,
            args: &[Value],
        ) -> Result<Value, NativeError> {
            let heap = ctx.interp_mut().string_heap_clone();
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::EncodeURI,
                args,
                &heap,
                ctx.heap(),
            )
            .map_err(|err| NativeError::TypeError {
                name: "encodeURI",
                reason: err.to_string(),
            })
        }
        fn global_encode_uri_component(
            ctx: &mut NativeCtx<'_>,
            args: &[Value],
        ) -> Result<Value, NativeError> {
            let heap = ctx.interp_mut().string_heap_clone();
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::EncodeURIComponent,
                args,
                &heap,
                ctx.heap(),
            )
            .map_err(|err| NativeError::TypeError {
                name: "encodeURIComponent",
                reason: err.to_string(),
            })
        }
        fn global_decode_uri(
            ctx: &mut NativeCtx<'_>,
            args: &[Value],
        ) -> Result<Value, NativeError> {
            let heap = ctx.interp_mut().string_heap_clone();
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::DecodeURI,
                args,
                &heap,
                ctx.heap(),
            )
            .map_err(|err| match err {
                crate::VmError::TypeError { message } => NativeError::TypeError {
                    name: "decodeURI",
                    reason: message,
                },
                other => NativeError::TypeError {
                    name: "decodeURI",
                    reason: other.to_string(),
                },
            })
        }
        fn global_decode_uri_component(
            ctx: &mut NativeCtx<'_>,
            args: &[Value],
        ) -> Result<Value, NativeError> {
            let heap = ctx.interp_mut().string_heap_clone();
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::DecodeURIComponent,
                args,
                &heap,
                ctx.heap(),
            )
            .map_err(|err| match err {
                crate::VmError::TypeError { message } => NativeError::TypeError {
                    name: "decodeURIComponent",
                    reason: message,
                },
                other => NativeError::TypeError {
                    name: "decodeURIComponent",
                    reason: other.to_string(),
                },
            })
        }

        // §B.2.1.1 / §B.2.1.2 — AnnexB legacy `escape` / `unescape`
        // globals. Same dispatcher path as the URI quartet above.
        fn global_escape(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            let heap = ctx.interp_mut().string_heap_clone();
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::Escape,
                args,
                &heap,
                ctx.heap(),
            )
            .map_err(|err| NativeError::TypeError {
                name: "escape",
                reason: err.to_string(),
            })
        }
        fn global_unescape(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            let heap = ctx.interp_mut().string_heap_clone();
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::Unescape,
                args,
                &heap,
                ctx.heap(),
            )
            .map_err(|err| NativeError::TypeError {
                name: "unescape",
                reason: err.to_string(),
            })
        }

        // §19.4.1 global `eval` — when invoked indirectly (e.g.
        // `(0, eval)(src)` / `var f = eval; f(src)`), the spec runs
        // §19.4.1.1 PerformEval with `direct = false`, which drops
        // the caller's lexical scope and never inherits strictness.
        // The runtime `Op::Eval` opcode already implements this for
        // the direct-call shape; the global binding reuses the same
        // entry point so reflective access works.
        // <https://tc39.es/ecma262/#sec-eval-x>
        fn global_eval(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            let arg = args.first().cloned().unwrap_or(Value::Undefined);
            ctx.interp_mut()
                .run_eval(&arg, false)
                .map_err(|err| NativeError::TypeError {
                    name: "eval",
                    reason: err.to_string(),
                })
        }

        let global_methods: &[(&'static str, u8, crate::native_function::NativeFastFn)] = &[
            ("parseInt", 2, number_parse_int_native),
            ("parseFloat", 1, number_parse_float_native),
            ("isNaN", 1, number_is_nan_native),
            ("isFinite", 1, number_is_finite_native),
            ("encodeURI", 1, global_encode_uri),
            ("encodeURIComponent", 1, global_encode_uri_component),
            ("decodeURI", 1, global_decode_uri),
            ("decodeURIComponent", 1, global_decode_uri_component),
            ("escape", 1, global_escape),
            ("unescape", 1, global_unescape),
            ("eval", 1, global_eval),
        ];
        let mut global_builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            global,
            vec![statics_root, prototype_root2],
        );
        for (name, length, call) in global_methods {
            global_builder.method(
                name,
                *length,
                NativeCall::Static(*call),
                Attr::builtin_function(),
            )?;
        }
    }

    let number_value = Value::Object(statics);
    // §21.1.3.1 `Number.prototype.constructor` points back at the
    // Number constructor.
    let _ = object::define_own_property(
        prototype,
        heap,
        "constructor",
        crate::object::PropertyDescriptor::data(number_value.clone(), true, false, true),
    );
    define_global(global, heap, "Number", number_value);
    // §21.1.2.{12,13} / §19.2.{4,5} — `Number.parseInt`,
    // `Number.parseFloat`, `Number.isNaN`, `Number.isFinite` MUST be
    // the same function object as their global-scope counterparts.
    // The two install passes above each created fresh
    // NativeFunctions; overwrite the `Number.*` slots with the global
    // bindings so identity (`Number.parseInt === parseInt`) holds.
    for shared in ["parseInt", "parseFloat", "isNaN", "isFinite"] {
        if let Some(global_fn) = object::get(global, heap, shared) {
            object::set(statics, heap, shared, global_fn);
        }
    }
    Ok(())
}

// `String` installer migrated to [`crate::string::intrinsic::Intrinsic`]
// — see [`crate::intrinsic_install::BuiltinIntrinsic`] for the
// per-class installation contract.

// `Boolean` installer migrated to
// [`crate::boolean::intrinsic::Intrinsic`].

fn install_function(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCtx, NativeError};

    fn function_prototype_call(
        _ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        Ok(Value::Undefined)
    }

    fn function_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let new_target_proto = native_new_target_prototype(ctx, "Function")?;
        let (interp, context) = ctx.interp_mut_and_context();
        let Some(context) = context else {
            return Err(NativeError::TypeError {
                name: "Function",
                reason: "missing execution context for Function constructor".to_string(),
            });
        };
        let result = interp
            .build_function_constructor_with_roots(&context, args, None, &[], &[args])
            .map_err(|err| {
                let reason = format!("{err}");
                match err {
                    crate::VmError::SyntaxError { .. } => NativeError::SyntaxError {
                        name: "Function",
                        reason,
                    },
                    _ => NativeError::TypeError {
                        name: "Function",
                        reason,
                    },
                }
            })?;
        if let (Value::NativeFunction(native), Some(proto)) = (&result, new_target_proto) {
            native.set_prototype_override(interp.gc_heap_mut(), Some(proto));
        }
        Ok(result)
    }

    let global_root = Value::Object(global);
    let function = alloc_object_with_value_roots(heap, &[&global_root])?;
    let function_root = Value::Object(function);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &function_root])?;
    let prototype_root = Value::Object(prototype);
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    object::set_prototype(function, heap, Some(prototype));
    let ctor_native = native_constructor_static_with_value_roots(
        heap,
        "Function",
        1,
        function_ctor_call,
        &[&global_root, &function_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_native_root = Value::NativeFunction(ctor_native);
    object::set_constructor_native(function, heap, ctor_native_root.clone());
    let prototype_call = native_static_with_value_roots(
        heap,
        "",
        0,
        function_prototype_call,
        &[
            &global_root,
            &function_root,
            &prototype_root,
            &ctor_native_root,
        ],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_call_native(prototype, heap, Value::NativeFunction(prototype_call));
    let length = PropertyDescriptor::data(
        Value::Number(crate::number::NumberValue::from_i32(1)),
        false,
        false,
        true,
    );
    let _ = object::define_own_property(function, heap, "length", length);
    let name_value = Value::String(
        crate::string::JsString::from_str("Function", &crate::string::StringHeap::default())
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let name = PropertyDescriptor::data(name_value, false, false, true);
    let _ = object::define_own_property(function, heap, "name", name);
    let prototype_descriptor =
        PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    let _ = object::define_own_property(function, heap, "prototype", prototype_descriptor);
    let prototype_length = PropertyDescriptor::data(
        Value::Number(crate::number::NumberValue::from_i32(0)),
        false,
        false,
        true,
    );
    let _ = object::define_own_property(prototype, heap, "length", prototype_length);
    let prototype_name_value = Value::String(
        crate::string::JsString::from_str("", &crate::string::StringHeap::default())
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );
    let prototype_name = PropertyDescriptor::data(prototype_name_value, false, false, true);
    let _ = object::define_own_property(prototype, heap, "name", prototype_name);
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root.clone(), function_root.clone()],
        );
        for method in function_prototype::FUNCTION_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    function_prototype::install_restricted_accessors(
        heap,
        prototype,
        &[&global_root, &function_root],
    )?;
    let constructor = PropertyDescriptor::data(Value::Object(function), true, false, true);
    let _ = object::define_own_property(prototype, heap, "constructor", constructor);
    define_global(global, heap, "Function", Value::Object(function));
    Ok(())
}

// `Math` installer migrated to [`crate::math::Intrinsic`].
// `JSON` installer migrated to [`crate::json::Intrinsic`].

fn install_object(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCtx, NativeError};

    /// §20.1.1.1 Object ( [ value ] ).
    ///
    /// 1. If `NewTarget` is neither `undefined` nor the active
    ///    `Object` function, return `OrdinaryCreateFromConstructor(NewTarget,
    ///    %Object.prototype%)`. (Subclass path — `class C extends Object {}`.)
    /// 2. If `value` is `undefined` or `null`, return
    ///    `OrdinaryObjectCreate(%Object.prototype%)`.
    /// 3. Return `! ToObject(value)`.
    ///
    /// `ToObject(value)` wraps a primitive with the appropriate
    /// `[[BooleanData]]` / `[[NumberData]]` / `[[StringData]]` /
    /// `[[SymbolData]]` / `[[BigIntData]]` slot so the wrapper's
    /// inherited `toString` / `valueOf` observe the original value.
    /// Object-typed operands return as-is.
    ///
    /// <https://tc39.es/ecma262/#sec-object-value>
    fn object_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        if ctx.is_construct_call() && !matches!(ctx.new_target(), Some(Value::Object(_))) {
            return Ok(ctx.this_value().clone());
        }
        match args.first() {
            None | Some(Value::Undefined | Value::Null) => {
                let obj = ctx.alloc_object().map_err(|_| NativeError::TypeError {
                    name: "Object",
                    reason: "object allocation failed".to_string(),
                })?;
                let interp = ctx.interp_mut();
                if let Ok(Value::Object(proto)) = interp.constructor_prototype_value("Object") {
                    crate::object::set_prototype(obj, &mut interp.gc_heap, Some(proto));
                }
                Ok(Value::Object(obj))
            }
            Some(value) => {
                // §7.1.18 ToObject — wrap a primitive with its
                // %X.prototype% and the matching internal data slot
                // (so the wrapper's inherited `toString` / `valueOf`
                // unbox correctly). Object-typed operands fall through
                // and return unchanged.
                let v = value.clone();
                match &v {
                    Value::Boolean(b) => {
                        let b = *b;
                        let interp = ctx.interp_mut();
                        let proto =
                            interp
                                .primitive_wrapper_prototype("Boolean")
                                .map_err(|err| NativeError::TypeError {
                                    name: "Object",
                                    reason: err.to_string(),
                                })?;
                        let obj = interp
                            .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                            .map_err(|err| NativeError::TypeError {
                                name: "Object",
                                reason: err.to_string(),
                            })?;
                        crate::object::set_boolean_data(obj, &mut interp.gc_heap, b);
                        Ok(Value::Object(obj))
                    }
                    Value::Number(n) => {
                        let n = *n;
                        let interp = ctx.interp_mut();
                        let proto =
                            interp
                                .primitive_wrapper_prototype("Number")
                                .map_err(|err| NativeError::TypeError {
                                    name: "Object",
                                    reason: err.to_string(),
                                })?;
                        let obj = interp
                            .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                            .map_err(|err| NativeError::TypeError {
                                name: "Object",
                                reason: err.to_string(),
                            })?;
                        crate::object::set_number_data(obj, &mut interp.gc_heap, n);
                        Ok(Value::Object(obj))
                    }
                    Value::String(s) => {
                        let s = s.clone();
                        let interp = ctx.interp_mut();
                        let proto =
                            interp
                                .primitive_wrapper_prototype("String")
                                .map_err(|err| NativeError::TypeError {
                                    name: "Object",
                                    reason: err.to_string(),
                                })?;
                        let obj = interp
                            .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                            .map_err(|err| NativeError::TypeError {
                                name: "Object",
                                reason: err.to_string(),
                            })?;
                        crate::object::set_string_data(obj, &mut interp.gc_heap, s);
                        Ok(Value::Object(obj))
                    }
                    Value::Symbol(sym) => {
                        let sym = sym.clone();
                        let interp = ctx.interp_mut();
                        let proto =
                            interp
                                .primitive_wrapper_prototype("Symbol")
                                .map_err(|err| NativeError::TypeError {
                                    name: "Object",
                                    reason: err.to_string(),
                                })?;
                        let obj = interp
                            .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                            .map_err(|err| NativeError::TypeError {
                                name: "Object",
                                reason: err.to_string(),
                            })?;
                        crate::object::set_symbol_data(obj, &mut interp.gc_heap, sym);
                        Ok(Value::Object(obj))
                    }
                    Value::BigInt(bigint) => {
                        let bigint = *bigint;
                        let interp = ctx.interp_mut();
                        let proto =
                            interp
                                .primitive_wrapper_prototype("BigInt")
                                .map_err(|err| NativeError::TypeError {
                                    name: "Object",
                                    reason: err.to_string(),
                                })?;
                        let obj = interp
                            .alloc_runtime_rooted_object_with_proto(proto, &[&v], &[])
                            .map_err(|err| NativeError::TypeError {
                                name: "Object",
                                reason: err.to_string(),
                            })?;
                        crate::object::set_bigint_data(obj, &mut interp.gc_heap, bigint);
                        Ok(Value::Object(obj))
                    }
                    _ => Ok(v),
                }
            }
        }
    }

    let global_root = Value::Object(global);
    let object = alloc_object_with_value_roots(heap, &[&global_root])?;
    let object_root = Value::Object(object);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &object_root])?;
    let prototype_root = Value::Object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "Object",
        1,
        object_ctor_call,
        &[&global_root, &object_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(object, heap, Value::NativeFunction(ctor_native));
    let length_desc =
        PropertyDescriptor::data(Value::Number(NumberValue::from_i32(1)), false, false, true);
    if !object::define_own_property(object, heap, "length", length_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("length"));
    }
    let bootstrap_string_heap = crate::StringHeap::default();
    let name_value = crate::JsString::from_latin1(b"Object", &bootstrap_string_heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let name_desc = PropertyDescriptor::data(Value::String(name_value), false, false, true);
    if !object::define_own_property(object, heap, "name", name_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("name"));
    }
    let prototype_desc = PropertyDescriptor::data(Value::Object(prototype), false, false, false);
    if !object::define_own_property(object, heap, "prototype", prototype_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            object,
            vec![global_root.clone(), prototype_root],
        );
        for method in object_statics::OBJECT_SPEC.methods {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root.clone()]);
        for method in object_statics::OBJECT_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    // §B.2.2.1 Object.prototype.__proto__ — accessor pair.
    // <https://tc39.es/ecma262/#sec-object.prototype.__proto__>
    {
        let proto_root = Value::Object(prototype);
        let getter = native_static_with_value_roots(
            heap,
            "get __proto__",
            0,
            object_statics::native_prototype_proto_get,
            &[&global_root, &proto_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let setter = native_static_with_value_roots(
            heap,
            "set __proto__",
            1,
            object_statics::native_prototype_proto_set,
            &[&global_root, &proto_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let desc = PropertyDescriptor::accessor(
            Some(Value::NativeFunction(getter)),
            Some(Value::NativeFunction(setter)),
            false,
            true,
        );
        if !object::define_own_property(prototype, heap, "__proto__", desc) {
            return Err(JsSurfaceError::DefinePropertyFailed("__proto__"));
        }
    }
    let _ = object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(Value::Object(object), true, false, true),
    );
    define_global(global, heap, "Object", Value::Object(object));
    Ok(())
}

fn install_date(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::js_surface::MethodSpec;
    use crate::native_function::NativeCall;
    use crate::{JsString, NativeCtx, NativeError};

    // §21.4.3 Date statics — trampolines that route to the typed
    // dispatcher with no `this`. The constructor's
    // `[[Construct]]` / `[[Call]]` slot still handles the
    // `Date(...)` and `new Date(...)` shapes via `date_ctor_call`.
    fn date_now_call(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
        crate::date::dispatch::call_static(otter_bytecode::method_id::DateMethod::Now, &[]).map_err(
            |err| NativeError::TypeError {
                name: "Date.now",
                reason: err.to_string(),
            },
        )
    }
    fn date_parse_call(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        crate::date::dispatch::call_static(otter_bytecode::method_id::DateMethod::Parse, args)
            .map_err(|err| NativeError::TypeError {
                name: "Date.parse",
                reason: err.to_string(),
            })
    }
    fn date_utc_call(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        crate::date::dispatch::call_static(otter_bytecode::method_id::DateMethod::UTC, args)
            .map_err(|err| NativeError::TypeError {
                name: "Date.UTC",
                reason: err.to_string(),
            })
    }

    fn date_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let time = {
            let heap = ctx.heap_mut();
            crate::date::dispatch::construct_time_value(args, heap)
        };
        if ctx.is_construct_call() {
            // §21.4.2.1 — `new Date(...)`. The construct receiver
            // is already a freshly allocated JsObject (via
            // OrdinaryCreateFromConstructor on `Date`). Install
            // the `[[DateValue]]` internal slot and return it.
            if let Value::Object(obj) = ctx.this_value().clone() {
                crate::object::set_date_data(obj, ctx.heap_mut(), time);
                return Ok(Value::Object(obj));
            }
            return Err(NativeError::TypeError {
                name: "Date",
                reason: "expected object receiver in `new Date(...)`".to_string(),
            });
        }
        // §21.4.2.2 — `Date()` without `new` returns the current
        // time rendered as an ISO string.
        let text = crate::date::to_iso_string(time).unwrap_or_else(|| "Invalid Date".to_string());
        let string_heap = ctx.interp_mut().string_heap_clone();
        let value =
            JsString::from_str(&text, &string_heap).map_err(|err| NativeError::TypeError {
                name: "Date",
                reason: err.to_string(),
            })?;
        Ok(Value::String(value))
    }

    let global_root = Value::Object(global);
    let constructor = alloc_object_with_value_roots(heap, &[&global_root])?;
    let constructor_root = Value::Object(constructor);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &constructor_root])?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(constructor, heap, Some(object_proto));
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    let prototype_root = Value::Object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "Date",
        7,
        date_ctor_call,
        &[&global_root, &constructor_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(constructor, heap, Value::NativeFunction(ctor_native));
    let _ = object::define_own_property(
        constructor,
        heap,
        "prototype",
        PropertyDescriptor::data(Value::Object(prototype), false, false, false),
    );

    // §21.4.4 Properties of the Date Prototype Object — install
    // JS-visible prototype method specs so `(new Date()).getTime`
    // resolves to a callable. The compile-time `CallDate` opcode
    // keeps using the prototype intrinsic table directly.
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root.clone(), constructor_root.clone()],
        );
        for spec in crate::date::prototype::DATE_PROTOTYPE_METHODS
            .iter()
            .chain(crate::date::prototype::DATE_PROTOTYPE_EXTRA_METHODS)
        {
            builder.method_from_spec(spec)?;
        }
    }

    // §21.4.3 statics — `Date.now()`, `Date.parse(str)`, `Date.UTC(...)`.
    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            constructor,
            vec![global_root.clone(), prototype_root.clone()],
        );
        builder.method_from_spec(&MethodSpec {
            name: "now",
            length: 0,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(date_now_call),
        })?;
        builder.method_from_spec(&MethodSpec {
            name: "parse",
            length: 1,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(date_parse_call),
        })?;
        builder.method_from_spec(&MethodSpec {
            name: "UTC",
            length: 7,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(date_utc_call),
        })?;
    }

    let date_value = Value::Object(constructor);
    let _ = object::define_own_property(
        prototype,
        heap,
        "constructor",
        PropertyDescriptor::data(date_value.clone(), true, false, true),
    );
    define_global(global, heap, "Date", date_value);
    Ok(())
}

// `Atomics` installer migrated to [`crate::atomics::Intrinsic`].
// `Reflect` installer migrated to [`crate::reflect::Intrinsic`].
// `console` installer migrated to [`crate::console::Intrinsic`].
// Timer globals migrated to [`crate::timers::Intrinsic`].

pub(crate) fn define_global_value(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    value: Value,
) {
    define_global(global, heap, name, value);
}

/// `BuiltinIntrinsic` adapters for the remaining built-ins whose
/// installers still live inside `bootstrap.rs`. Each adapter is a
/// zero-sized marker type wired through the per-class private
/// `install_*` body. Migration target: move the bodies into
/// per-class modules and drop these adapters; for now they cleanly
/// retire the `install: install_xxx` function-pointer entries from
/// `BOOTSTRAP_ENTRIES` without disturbing the installer bodies.
/// `BuiltinIntrinsic` adapter for the global `Object` constructor.
pub struct ObjectIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for ObjectIntrinsic {
    const NAME: &'static str = "Object";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_object(heap, global)
    }
}

/// `BuiltinIntrinsic` adapter for the global `Array` constructor.
pub struct ArrayIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for ArrayIntrinsic {
    const NAME: &'static str = "Array";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_array(heap, global)
    }
}

/// `BuiltinIntrinsic` adapter for the global `Number` constructor.
pub struct NumberIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for NumberIntrinsic {
    const NAME: &'static str = "Number";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_number(heap, global)
    }
}

/// `BuiltinIntrinsic` adapter for the global `Symbol` constructor.
pub struct SymbolIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for SymbolIntrinsic {
    const NAME: &'static str = "Symbol";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_symbol(heap, global)
    }
}

/// `BuiltinIntrinsic` adapter for the global `Date` constructor.
pub struct DateIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for DateIntrinsic {
    const NAME: &'static str = "Date";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_date(heap, global)
    }
}

/// `BuiltinIntrinsic` adapter for the global `Proxy` constructor.
pub struct ProxyIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for ProxyIntrinsic {
    const NAME: &'static str = "Proxy";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_proxy(heap, global)
    }
}

/// `BuiltinIntrinsic` adapter for the global `Function` constructor.
pub struct FunctionIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for FunctionIntrinsic {
    const NAME: &'static str = "Function";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_function(heap, global)
    }
}

/// Placeholder `BuiltinIntrinsic` for `Intl` — empty object with a
/// prototype slot. Real Intl integration ships separately.
pub struct IntlIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for IntlIntrinsic {
    const NAME: &'static str = "Intl";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_placeholder(Self::NAME, heap, global)
    }
}

/// Placeholder `BuiltinIntrinsic` for `Temporal`.
pub struct TemporalIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for TemporalIntrinsic {
    const NAME: &'static str = "Temporal";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_placeholder(Self::NAME, heap, global)
    }
}

/// Placeholder `BuiltinIntrinsic` for `AggregateError`.
pub struct AggregateErrorIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for AggregateErrorIntrinsic {
    const NAME: &'static str = "AggregateError";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_placeholder(Self::NAME, heap, global)
    }
}

/// §27.1.1.1 `Iterator()` constructor — abstract base for the
/// ES2025 iterator-helpers protocol. Direct calls / direct `new`
/// throw `TypeError`; subclass `super()` calls allocate an
/// ordinary object inheriting from the new-target's `.prototype`.
fn iterator_ctor_call(
    ctx: &mut crate::NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, crate::NativeError> {
    let new_target = ctx.new_target().cloned();
    let Some(new_target) = new_target else {
        return Err(crate::NativeError::TypeError {
            name: "Iterator",
            reason: "constructor Iterator requires 'new'".to_string(),
        });
    };
    // §27.1.1.1 step 2 — OrdinaryCreateFromConstructor(new.target,
    // "%Iterator.prototype%"). Resolve the prototype off the
    // new-target so subclass `class T extends Iterator { … }` /
    // `new T()` inherits from `T.prototype`.
    let new_target_root = new_target.clone();
    let obj = ctx
        .alloc_object_with_roots(&[&new_target_root], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator",
            reason: "out of memory".to_string(),
        })?;
    let proto = match &new_target {
        Value::NativeFunction(nf) => {
            let heap = ctx.heap();
            let string_heap = ctx.cx.interp.string_heap_clone();
            nf.own_property_descriptor(heap, &string_heap, "prototype")
                .ok()
                .flatten()
                .and_then(|d| match d.kind {
                    crate::object::DescriptorKind::Data { value } => Some(value),
                    _ => None,
                })
        }
        Value::Object(target_obj) => crate::object::get(*target_obj, ctx.heap(), "prototype"),
        Value::ClassConstructor(c) => {
            crate::object::get(c.statics(ctx.heap()), ctx.heap(), "prototype")
        }
        _ => None,
    };
    if let Some(Value::Object(proto_obj)) = proto {
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto_obj));
    }
    Ok(Value::Object(obj))
}

/// `BuiltinIntrinsic` for the ES2025 `Iterator` constructor — the
/// abstract base of the iterator-helpers protocol.
pub struct IteratorIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for IteratorIntrinsic {
    const NAME: &'static str = "Iterator";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        let global_root = Value::Object(global);
        // Build the spec-compliant Iterator constructor (callable as
        // `new Iterator()` via subclass `super()`, throws when
        // invoked directly).
        let ctor = native_constructor_static_with_value_roots(
            heap,
            "Iterator",
            0,
            iterator_ctor_call,
            &[&global_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        // Wire %Iterator.prototype% — a fresh object chained later
        // to %Object.prototype% and decorated with the iterator
        // helpers below.
        let proto = alloc_object_with_value_roots(heap, &[&global_root])?;
        let proto_value = Value::Object(proto);
        let string_heap = crate::string::StringHeap::default();
        let prototype_desc =
            crate::object::PropertyDescriptor::data(proto_value.clone(), false, false, false);
        if !ctor.define_own_property(heap, &string_heap, "prototype", prototype_desc) {
            return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
        }
        // §27.1.2 — `Iterator.prototype.constructor = %Iterator%`,
        // writable / non-enumerable / configurable.
        crate::object::define_own_property(
            proto,
            heap,
            "constructor",
            crate::object::PropertyDescriptor::data(Value::NativeFunction(ctor), true, false, true),
        );
        define_global_value(global, heap, Self::NAME, Value::NativeFunction(ctor));
        let iterator_ctor = ctor;
        let ctor_root = Value::NativeFunction(iterator_ctor);
        let from_fn =
            native_static_with_value_roots(heap, "from", 1, iterator_from_native, &[&ctor_root])
                .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let from_desc = crate::object::PropertyDescriptor::data(
            Value::NativeFunction(from_fn),
            true,
            false,
            true,
        );
        let _ = iterator_ctor.define_own_property(heap, &string_heap, "from", from_desc);
        let prototype = proto;
        // §27.1.2 %IteratorPrototype% — install the iterator-helpers
        // proposal methods on the prototype carried by the
        // `Iterator` constructor. The handlers re-enter the
        // runtime via existing `IteratorState` wrappers so the
        // call-method fast path and reflective property access
        // share behaviour.
        let proto_root = Value::Object(prototype);
        // §27.1.2: `%IteratorPrototype%.[[Prototype]]` is
        // `%Object.prototype%` so reflective walks
        // (`Object.getPrototypeOf(Iterator.prototype) === Object.prototype`)
        // terminate at the realm-wide Object root.
        if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
            && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
        {
            object::set_prototype(prototype, heap, Some(object_proto));
        }
        let install_proto = |heap: &mut otter_gc::GcHeap,
                             name: &'static str,
                             length: u8,
                             call: crate::native_function::NativeFastFn|
         -> Result<(), JsSurfaceError> {
            let f = native_static_with_value_roots(heap, name, length, call, &[&proto_root])
                .map_err(|_| JsSurfaceError::OutOfMemory)?;
            object::set(prototype, heap, name, Value::NativeFunction(f));
            Ok(())
        };
        install_proto(heap, "map", 1, iterator_proto_map)?;
        install_proto(heap, "filter", 1, iterator_proto_filter)?;
        install_proto(heap, "take", 1, iterator_proto_take)?;
        install_proto(heap, "drop", 1, iterator_proto_drop)?;
        install_proto(heap, "flatMap", 1, iterator_proto_flat_map)?;
        install_proto(heap, "toArray", 0, iterator_proto_to_array)?;
        install_proto(heap, "forEach", 1, iterator_proto_for_each)?;
        install_proto(heap, "reduce", 1, iterator_proto_reduce)?;
        install_proto(heap, "some", 1, iterator_proto_some)?;
        install_proto(heap, "every", 1, iterator_proto_every)?;
        install_proto(heap, "find", 1, iterator_proto_find)?;
        // §27.1.5.1 / §22.1.5.1 / §23.1.5.1 / §24.1.5.1 / §24.2.5.1 —
        // Otter exposes a single `%IteratorPrototype%` that carries
        // `next`, `return`, and `throw`; the per-kind iterator
        // sub-prototypes (Map, Set, String, Array) inherit from it.
        // Each method routes back through `iterator_next_full` /
        // `iterator_helper_dispatch` so the spec result record is
        // identical whether the call comes from `Op::IteratorNext`
        // or reflective `proto.next.call(it)`.
        install_proto(heap, "next", 0, iterator_proto_next)?;
        install_proto(heap, "return", 1, iterator_proto_return)?;
        install_proto(heap, "throw", 1, iterator_proto_throw)?;
        Ok(())
    }
}

/// §27.1.2.1 `Iterator.prototype[@@iterator]` — returns the
/// receiver unchanged so any iterator value is itself iterable.
///
/// <https://tc39.es/ecma262/#sec-iteratorprototype-%symbol.iterator%>
fn iterator_proto_symbol_iterator(
    ctx: &mut crate::NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, crate::NativeError> {
    Ok(ctx.this_value().clone())
}

/// Post-bootstrap pass that installs the symbol-keyed members of
/// `%Iterator.prototype%`: `@@iterator` (returns `this`) and
/// `@@toStringTag = "Iterator"`. Runs after the well-known symbol
/// table is materialised in the same phase that installs
/// `@@toStringTag` on TypedArray prototypes.
pub fn install_iterator_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;
    let prototype = match object::get(global, heap, "Iterator") {
        Some(Value::NativeFunction(ctor)) => ctor
            .own_property_descriptor(heap, string_heap, "prototype")
            .ok()
            .flatten()
            .and_then(|d| match d.kind {
                crate::object::DescriptorKind::Data {
                    value: Value::Object(p),
                } => Some(p),
                _ => None,
            }),
        Some(Value::Object(iterator_ctor)) => match object::get(iterator_ctor, heap, "prototype") {
            Some(Value::Object(p)) => Some(p),
            _ => None,
        },
        _ => None,
    };
    let Some(prototype) = prototype else {
        return Ok(());
    };
    let proto_root = Value::Object(prototype);
    let symbol_iter_fn = native_static_with_value_roots(
        heap,
        "[Symbol.iterator]",
        0,
        iterator_proto_symbol_iterator,
        &[&proto_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let iter_sym = well_known.get(WellKnown::Iterator);
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &iter_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::NativeFunction(symbol_iter_fn)),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    let tag_sym = well_known.get(WellKnown::ToStringTag);
    let tag = crate::string::JsString::from_str("Iterator", string_heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        &tag_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::String(tag)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

/// Per-kind iterator prototypes built off `%IteratorPrototype%`.
/// Returned from [`build_builtin_iterator_prototypes_post_bootstrap`]
/// so the caller can stash them on the [`crate::Interpreter`] cache
/// used by `intrinsic_prototype_object_for(Value::Iterator(_))`.
pub struct BuiltinIteratorPrototypes {
    /// `%ArrayIteratorPrototype%` — §23.1.5.2.
    pub array: JsObject,
    /// `%MapIteratorPrototype%` — §24.1.5.2.
    pub map: JsObject,
    /// `%SetIteratorPrototype%` — §24.2.5.2.
    pub set: JsObject,
    /// `%StringIteratorPrototype%` — §22.1.5.2.
    pub string: JsObject,
    /// `%RegExpStringIteratorPrototype%` — §22.2.7.2.
    pub regexp_string: JsObject,
}

/// §22.1.5.2 / §23.1.5.2 / §24.1.5.2 / §24.2.5.2 — materialise the
/// per-kind iterator prototypes. Each inherits from
/// `%IteratorPrototype%` and carries its own `@@toStringTag`. Caller
/// (`Interpreter::new`) caches the resulting `JsObject`s so
/// `[[GetPrototypeOf]]` on a `Value::Iterator` routes to the right
/// per-kind prototype.
pub fn build_builtin_iterator_prototypes_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    string_heap: &crate::string::StringHeap,
    shape_root: object::ShapeHandle,
    parent: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<BuiltinIteratorPrototypes, JsSurfaceError> {
    use crate::symbol::WellKnown;
    let parent_value = Value::Object(parent);
    let tag_sym = well_known.get(WellKnown::ToStringTag);
    let mut make = |tag: &'static str| -> Result<JsObject, JsSurfaceError> {
        let proto = {
            let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                parent_value.trace_value_slots(visitor);
            };
            object::alloc_object_with_shape_roots(heap, shape_root, &mut visit)
                .map_err(|_| JsSurfaceError::OutOfMemory)?
        };
        object::set_prototype(proto, heap, Some(parent));
        let tag_string = crate::string::JsString::from_str(tag, string_heap)
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
        object::define_own_symbol_property_partial(
            proto,
            heap,
            &tag_sym,
            object::PartialPropertyDescriptor {
                value: Some(Value::String(tag_string)),
                writable: Some(false),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
        Ok(proto)
    };
    let array = make("Array Iterator")?;
    let map = make("Map Iterator")?;
    let set = make("Set Iterator")?;
    let string = make("String Iterator")?;
    let regexp_string = make("RegExp String Iterator")?;
    let next_fn = native_static_with_value_roots(
        heap,
        "next",
        0,
        regexp_string_iterator_proto_next,
        &[&parent_value],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    if !object::define_own_property(
        regexp_string,
        heap,
        "next",
        object::PropertyDescriptor::data(Value::NativeFunction(next_fn), true, false, true),
    ) {
        return Err(JsSurfaceError::DefinePropertyFailed(
            "RegExpStringIteratorPrototype.next",
        ));
    }
    Ok(BuiltinIteratorPrototypes {
        array,
        map,
        set,
        string,
        regexp_string,
    })
}

fn iterator_receiver(
    ctx: &mut crate::NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::IteratorHandle, crate::NativeError> {
    let this_value = ctx.this_value().clone();
    match this_value {
        Value::Iterator(h) => Ok(h),
        // §27.1.2 step 1 — `Iterator.prototype.X.call(generator, ...)`
        // accepts a Generator receiver by wrapping it into
        // `IteratorState::Generator` so the lazy combinators and
        // eager terminals share the iterator dispatch path.
        Value::Generator(g) => {
            let gen_value = Value::Generator(g);
            let state = crate::IteratorState::Generator { handle: g };
            ctx.alloc_iterator_state(state, &[&gen_value], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name,
                    reason: "iterator allocation failed".to_string(),
                })
        }
        // §27.1.4.1.1 GetIteratorDirect — the `Iterator.prototype.X`
        // helpers (drop / take / map / filter / forEach / every /
        // some / find / reduce / toArray / flatMap) accept any
        // receiver that implements the iterator protocol via a
        // callable `next` method; they do NOT walk `@@iterator`.
        // The original wrapper required a built-in iterator-shaped
        // Value, so plain-object iterator-protocol receivers
        // (`{ next: () => …, return: () => … }`) were rejected
        // with "this is not an iterator". Probe `next` directly and
        // wrap as `IteratorState::User`.
        // <https://tc39.es/ecma262/#sec-getiteratordirect>
        Value::Object(_) | Value::Map(_) | Value::Set(_) | Value::Array(_) | Value::String(_) => {
            let (interp, exec_ctx) = ctx.interp_mut_and_context();
            let exec_ctx = exec_ctx.ok_or_else(|| crate::NativeError::TypeError {
                name,
                reason: "missing execution context".to_string(),
            })?;
            let next_key = crate::VmPropertyKey::String("next");
            let outcome = interp
                .ordinary_get_value(
                    &exec_ctx,
                    this_value.clone(),
                    this_value.clone(),
                    &next_key,
                    0,
                )
                .map_err(|e| crate::NativeError::TypeError {
                    name,
                    reason: e.to_string(),
                })?;
            let next_method = match outcome {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                    interp
                        .run_callable_sync(&exec_ctx, &getter, this_value.clone(), args)
                        .map_err(|e| crate::NativeError::TypeError {
                            name,
                            reason: e.to_string(),
                        })?
                }
            };
            if !interp.is_callable_runtime(&next_method) {
                return Err(crate::NativeError::TypeError {
                    name,
                    reason: "this is not an iterator".to_string(),
                });
            }
            let this_root = this_value.clone();
            let state = crate::IteratorState::User {
                iterator: this_value,
            };
            ctx.alloc_iterator_state(state, &[&this_root], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name,
                    reason: "iterator allocation failed".to_string(),
                })
        }
        _ => Err(crate::NativeError::TypeError {
            name,
            reason: "this is not an iterator".to_string(),
        }),
    }
}

/// Strict iterator receiver used by `%IteratorPrototype%.next/return/throw`.
/// Only accepts built-in iterator-shaped values (`Value::Iterator`,
/// `Value::Generator`); non-iterator receivers throw `TypeError` per
/// §27.1.5.1.2 step 2. The looser [`iterator_receiver`] used by the
/// helper terminals (map/filter/drop/take/forEach/…) would re-wrap an
/// ordinary object as `IteratorState::User`, which causes infinite
/// re-entry when the wrapped object's `next` resolves back to this
/// same native through the prototype chain.
fn iterator_receiver_builtin(
    ctx: &mut crate::NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::IteratorHandle, crate::NativeError> {
    let this_value = ctx.this_value().clone();
    match this_value {
        Value::Iterator(h) => Ok(h),
        Value::Generator(g) => {
            let gen_value = Value::Generator(g);
            let state = crate::IteratorState::Generator { handle: g };
            ctx.alloc_iterator_state(state, &[&gen_value], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name,
                    reason: "iterator allocation failed".to_string(),
                })
        }
        _ => Err(crate::NativeError::TypeError {
            name,
            reason: "this is not a built-in iterator".to_string(),
        }),
    }
}

fn require_callable_arg(
    ctx: &crate::NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
    index: usize,
) -> Result<Value, crate::NativeError> {
    let v = args.get(index).cloned().unwrap_or(Value::Undefined);
    if ctx.cx.interp.is_callable_runtime(&v) {
        Ok(v)
    } else {
        Err(crate::NativeError::TypeError {
            name,
            reason: "argument must be callable".to_string(),
        })
    }
}

fn iterator_proto_map(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let source = iterator_receiver(ctx, "Iterator.prototype.map")?;
    let mapper = require_callable_arg(ctx, args, "Iterator.prototype.map", 0)?;
    let source_value = Value::Iterator(source);
    let state = crate::IteratorState::Map {
        source,
        mapper: mapper.clone(),
    };
    let handle = ctx
        .alloc_iterator_state(state, &[&source_value, &mapper], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.map",
            reason: "iterator allocation failed".to_string(),
        })?;
    Ok(Value::Iterator(handle))
}

fn iterator_proto_filter(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let source = iterator_receiver(ctx, "Iterator.prototype.filter")?;
    let predicate = require_callable_arg(ctx, args, "Iterator.prototype.filter", 0)?;
    let source_value = Value::Iterator(source);
    let state = crate::IteratorState::Filter {
        source,
        predicate: predicate.clone(),
    };
    let handle = ctx
        .alloc_iterator_state(state, &[&source_value, &predicate], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.filter",
            reason: "iterator allocation failed".to_string(),
        })?;
    Ok(Value::Iterator(handle))
}

fn iterator_proto_take(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let source = iterator_receiver(ctx, "Iterator.prototype.take")?;
    let n = iterator_arg_count_native(ctx, args, "Iterator.prototype.take")?;
    let source_value = Value::Iterator(source);
    let state = crate::IteratorState::Take {
        source,
        remaining: n,
    };
    let handle = ctx
        .alloc_iterator_state(state, &[&source_value], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.take",
            reason: "iterator allocation failed".to_string(),
        })?;
    Ok(Value::Iterator(handle))
}

fn iterator_proto_drop(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let source = iterator_receiver(ctx, "Iterator.prototype.drop")?;
    let n = iterator_arg_count_native(ctx, args, "Iterator.prototype.drop")?;
    let source_value = Value::Iterator(source);
    let state = crate::IteratorState::Drop { source, to_drop: n };
    let handle = ctx
        .alloc_iterator_state(state, &[&source_value], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.drop",
            reason: "iterator allocation failed".to_string(),
        })?;
    Ok(Value::Iterator(handle))
}

fn iterator_proto_flat_map(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let source = iterator_receiver(ctx, "Iterator.prototype.flatMap")?;
    let mapper = require_callable_arg(ctx, args, "Iterator.prototype.flatMap", 0)?;
    let source_value = Value::Iterator(source);
    let state = crate::IteratorState::FlatMap {
        source,
        mapper: mapper.clone(),
        inner: None,
    };
    let handle = ctx
        .alloc_iterator_state(state, &[&source_value, &mapper], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.flatMap",
            reason: "iterator allocation failed".to_string(),
        })?;
    Ok(Value::Iterator(handle))
}

fn iterator_arg_count_native(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<u64, crate::NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::Undefined);
    // §27.5.1.2 step 3 — `numLimit = ? ToNumber(limit)`. Non-
    // primitive operands route through `ToPrimitive(hint: number)`
    // so `valueOf` / `toString` / `Symbol.toPrimitive` hooks fire.
    let primitive = if crate::abstract_ops::is_primitive(&arg) {
        arg
    } else {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or_else(|| crate::NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
        interp
            .evaluate_to_primitive(&exec, &arg, crate::abstract_ops::ToPrimitiveHint::Number)
            .map_err(|e| crate::NativeError::TypeError {
                name,
                reason: e.to_string(),
            })?
    };
    let n = crate::number::to_number_value(&primitive);
    if n.is_nan() {
        return Err(crate::NativeError::RangeError {
            name,
            reason: "argument must be a non-negative integer".to_string(),
        });
    }
    let trunc = n.trunc();
    if trunc < 0.0 {
        return Err(crate::NativeError::RangeError {
            name,
            reason: "argument must be a non-negative integer".to_string(),
        });
    }
    if n.is_infinite() {
        return Ok(u64::MAX);
    }
    Ok(trunc as u64)
}

fn iterator_proto_to_array(
    ctx: &mut crate::NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, crate::NativeError> {
    let handle = iterator_receiver(ctx, "Iterator.prototype.toArray")?;
    let exec_ctx =
        ctx.execution_context()
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name: "Iterator.prototype.toArray",
                reason: "missing execution context".to_string(),
            })?;
    let mut collected: Vec<Value> = Vec::new();
    loop {
        let (v, done) = ctx
            .cx
            .interp
            .iterator_next_full(&exec_ctx, &handle)
            .map_err(|e| crate::NativeError::TypeError {
                name: "Iterator.prototype.toArray",
                reason: e.to_string(),
            })?;
        if done {
            break;
        }
        collected.push(v);
    }
    let iter_value = Value::Iterator(handle);
    let arr = ctx
        .array_from_elements_with_roots(
            collected.iter().cloned(),
            &[&iter_value],
            &[collected.as_slice()],
        )
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.toArray",
            reason: "array allocation failed".to_string(),
        })?;
    Ok(Value::Array(arr))
}

fn iterator_proto_for_each(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let handle = iterator_receiver(ctx, "Iterator.prototype.forEach")?;
    let callback = require_callable_arg(ctx, args, "Iterator.prototype.forEach", 0)?;
    let exec_ctx =
        ctx.execution_context()
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name: "Iterator.prototype.forEach",
                reason: "missing execution context".to_string(),
            })?;
    let mut idx: f64 = 0.0;
    loop {
        let (v, done) = ctx
            .cx
            .interp
            .iterator_next_full(&exec_ctx, &handle)
            .map_err(|e| crate::NativeError::TypeError {
                name: "Iterator.prototype.forEach",
                reason: e.to_string(),
            })?;
        if done {
            break;
        }
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(v);
        cb_args.push(Value::Number(crate::number::NumberValue::from_f64(idx)));
        ctx.cx
            .interp
            .run_callable_sync(&exec_ctx, &callback, Value::Undefined, cb_args)
            .map_err(|e| crate::NativeError::TypeError {
                name: "Iterator.prototype.forEach",
                reason: e.to_string(),
            })?;
        idx += 1.0;
    }
    Ok(Value::Undefined)
}

fn iterator_proto_reduce(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let handle = iterator_receiver(ctx, "Iterator.prototype.reduce")?;
    let reducer = require_callable_arg(ctx, args, "Iterator.prototype.reduce", 0)?;
    let exec_ctx =
        ctx.execution_context()
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name: "Iterator.prototype.reduce",
                reason: "missing execution context".to_string(),
            })?;
    let has_initial = args.len() >= 2;
    let mut acc = if has_initial {
        args[1].clone()
    } else {
        Value::Undefined
    };
    // `has_acc` flips to true once `acc` holds a real value — either
    // the caller-supplied initial value or the first yielded element
    // when no initial was passed.
    let mut has_acc = has_initial;
    let mut idx: f64 = 0.0;
    loop {
        let (v, done) = ctx
            .cx
            .interp
            .iterator_next_full(&exec_ctx, &handle)
            .map_err(|e| crate::NativeError::TypeError {
                name: "Iterator.prototype.reduce",
                reason: e.to_string(),
            })?;
        if done {
            break;
        }
        if !has_acc {
            acc = v;
            has_acc = true;
            idx += 1.0;
            continue;
        }
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(acc.clone());
        cb_args.push(v);
        cb_args.push(Value::Number(crate::number::NumberValue::from_f64(idx)));
        acc = ctx
            .cx
            .interp
            .run_callable_sync(&exec_ctx, &reducer, Value::Undefined, cb_args)
            .map_err(|e| crate::NativeError::TypeError {
                name: "Iterator.prototype.reduce",
                reason: e.to_string(),
            })?;
        idx += 1.0;
    }
    if !has_acc {
        return Err(crate::NativeError::TypeError {
            name: "Iterator.prototype.reduce",
            reason: "reduce of empty iterator with no initial value".to_string(),
        });
    }
    Ok(acc)
}

fn iterator_proto_some(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    iterator_predicate_drain(ctx, args, "Iterator.prototype.some", true, false)
}

fn iterator_proto_every(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    iterator_predicate_drain(ctx, args, "Iterator.prototype.every", false, true)
}

fn iterator_proto_find(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let handle = iterator_receiver(ctx, "Iterator.prototype.find")?;
    let predicate = require_callable_arg(ctx, args, "Iterator.prototype.find", 0)?;
    let exec_ctx =
        ctx.execution_context()
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name: "Iterator.prototype.find",
                reason: "missing execution context".to_string(),
            })?;
    let mut idx: f64 = 0.0;
    loop {
        let (v, done) = ctx
            .cx
            .interp
            .iterator_next_full(&exec_ctx, &handle)
            .map_err(|e| crate::NativeError::TypeError {
                name: "Iterator.prototype.find",
                reason: e.to_string(),
            })?;
        if done {
            break;
        }
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(v.clone());
        cb_args.push(Value::Number(crate::number::NumberValue::from_f64(idx)));
        let kept = ctx
            .cx
            .interp
            .run_callable_sync(&exec_ctx, &predicate, Value::Undefined, cb_args)
            .map_err(|e| crate::NativeError::TypeError {
                name: "Iterator.prototype.find",
                reason: e.to_string(),
            })?;
        if kept.to_boolean(ctx.heap()) {
            return Ok(v);
        }
        idx += 1.0;
    }
    Ok(Value::Undefined)
}

/// §27.1.5.1.2 `%IteratorPrototype%.next()` — drive one step on the
/// receiver iterator and wrap the (value, done) pair in the spec's
/// result record.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-%25iteratorprototype%25.next>
fn iterator_proto_next(
    ctx: &mut crate::NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, crate::NativeError> {
    let handle = iterator_receiver_builtin(ctx, "Iterator.prototype.next")?;
    let exec_ctx =
        ctx.execution_context()
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name: "Iterator.prototype.next",
                reason: "missing execution context".to_string(),
            })?;
    let iter_value = Value::Iterator(handle);
    let (value, done) = ctx
        .cx
        .interp
        .iterator_next_full(&exec_ctx, &handle)
        .map_err(|e| crate::NativeError::TypeError {
            name: "Iterator.prototype.next",
            reason: e.to_string(),
        })?;
    let obj = ctx
        .alloc_object_with_roots(&[&iter_value, &value], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.next",
            reason: "result allocation failed".to_string(),
        })?;
    ctx.set_property(obj, "value", value)
        .map_err(|e| crate::NativeError::TypeError {
            name: "Iterator.prototype.next",
            reason: e.to_string(),
        })?;
    ctx.set_property(obj, "done", Value::Boolean(done))
        .map_err(|e| crate::NativeError::TypeError {
            name: "Iterator.prototype.next",
            reason: e.to_string(),
        })?;
    Ok(Value::Object(obj))
}

/// §22.2.7.2 `%RegExpStringIteratorPrototype%.next`.
///
/// Unlike the generic `%IteratorPrototype%.next` helper, this own
/// method requires the receiver to carry RegExp String Iterator
/// internal state.
fn regexp_string_iterator_proto_next(
    ctx: &mut crate::NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, crate::NativeError> {
    let name = "RegExpStringIteratorPrototype.next";
    let this_value = ctx.this_value().clone();
    let Value::Iterator(handle) = this_value else {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "this is not a RegExp String Iterator".to_string(),
        });
    };
    let is_regexp_string = ctx
        .cx
        .interp
        .gc_heap_for_cx()
        .read_payload(handle, |state| {
            matches!(state, crate::IteratorState::RegExpString { .. })
        });
    if !is_regexp_string {
        return Err(crate::NativeError::TypeError {
            name,
            reason: "this is not a RegExp String Iterator".to_string(),
        });
    }
    let exec_ctx =
        ctx.execution_context()
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name,
                reason: "missing execution context".to_string(),
            })?;
    let iter_value = Value::Iterator(handle);
    let (value, done) = ctx
        .cx
        .interp
        .iterator_next_full(&exec_ctx, &handle)
        .map_err(|e| crate::NativeError::TypeError {
            name,
            reason: e.to_string(),
        })?;
    let obj = ctx
        .alloc_object_with_roots(&[&iter_value, &value], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name,
            reason: "result allocation failed".to_string(),
        })?;
    ctx.set_property(obj, "value", value)
        .map_err(|e| crate::NativeError::TypeError {
            name,
            reason: e.to_string(),
        })?;
    ctx.set_property(obj, "done", Value::Boolean(done))
        .map_err(|e| crate::NativeError::TypeError {
            name,
            reason: e.to_string(),
        })?;
    Ok(Value::Object(obj))
}

/// §27.1.5.1.3 `%IteratorPrototype%.return(value)` — mark the
/// receiver iterator exhausted and return the spec result record
/// `{ value, done: true }`. Generator-backed iterators delegate
/// to the generator's `return` handler through `iterator_helper_dispatch`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-%25iteratorprototype%25.return>
fn iterator_proto_return(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let handle = iterator_receiver_builtin(ctx, "Iterator.prototype.return")?;
    let arg = args.first().cloned().unwrap_or(Value::Undefined);
    let iter_value = Value::Iterator(handle);
    ctx.cx
        .interp
        .gc_heap_for_cx_mut()
        .with_payload(handle, |state| {
            *state = crate::IteratorState::Exhausted;
        });
    let obj = ctx
        .alloc_object_with_roots(&[&iter_value, &arg], &[])
        .map_err(|_| crate::NativeError::TypeError {
            name: "Iterator.prototype.return",
            reason: "result allocation failed".to_string(),
        })?;
    ctx.set_property(obj, "value", arg)
        .map_err(|e| crate::NativeError::TypeError {
            name: "Iterator.prototype.return",
            reason: e.to_string(),
        })?;
    ctx.set_property(obj, "done", Value::Boolean(true))
        .map_err(|e| crate::NativeError::TypeError {
            name: "Iterator.prototype.return",
            reason: e.to_string(),
        })?;
    Ok(Value::Object(obj))
}

/// §27.1.5.1.4 `%IteratorPrototype%.throw(value)` — propagate the
/// argument as a thrown completion. Built-in (non-generator)
/// iterators have no `[[Throw]]` handler, so the abstract algorithm
/// degrades to "throw value".
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-%25iteratorprototype%25.throw>
fn iterator_proto_throw(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let _handle = iterator_receiver_builtin(ctx, "Iterator.prototype.throw")?;
    let arg = args.first().cloned().unwrap_or(Value::Undefined);
    Err(crate::NativeError::Thrown {
        name: "Iterator.prototype.throw",
        message: arg.display_string(ctx.heap()),
    })
}

fn iterator_predicate_drain(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
    short_on_truthy: bool,
    initial: bool,
) -> Result<Value, crate::NativeError> {
    let handle = iterator_receiver(ctx, name)?;
    let predicate = require_callable_arg(ctx, args, name, 0)?;
    let exec_ctx =
        ctx.execution_context()
            .cloned()
            .ok_or_else(|| crate::NativeError::TypeError {
                name,
                reason: "missing execution context".to_string(),
            })?;
    let mut idx: f64 = 0.0;
    loop {
        let (v, done) = ctx
            .cx
            .interp
            .iterator_next_full(&exec_ctx, &handle)
            .map_err(|e| crate::NativeError::TypeError {
                name,
                reason: e.to_string(),
            })?;
        if done {
            return Ok(Value::Boolean(initial));
        }
        let mut cb_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
        cb_args.push(v);
        cb_args.push(Value::Number(crate::number::NumberValue::from_f64(idx)));
        let kept = ctx
            .cx
            .interp
            .run_callable_sync(&exec_ctx, &predicate, Value::Undefined, cb_args)
            .map_err(|e| crate::NativeError::TypeError {
                name,
                reason: e.to_string(),
            })?;
        if kept.to_boolean(ctx.heap()) == short_on_truthy {
            return Ok(Value::Boolean(short_on_truthy));
        }
        idx += 1.0;
    }
}

/// §27.1.4.1 `Iterator.from(iterable)` — wraps the operand into a
/// foundation iterator. Already-iterator inputs pass through;
/// iterable objects route through `GetIterator` so `[Symbol.iterator]()`
/// fires and the resulting user iterator is wrapped in
/// `IteratorState::User`. Plain Array / Set / Map / String operands
/// route through their respective `IteratorState` shapes via the
/// existing `make_*_iterator_factory` helpers. Primitive inputs that
/// aren't iterable raise TypeError per spec step 4.
fn iterator_from_native(
    ctx: &mut crate::NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, crate::NativeError> {
    let input = args.first().cloned().unwrap_or(Value::Undefined);
    match &input {
        Value::Iterator(_) => Ok(input),
        Value::Array(arr) => {
            let arr_value = Value::Array(*arr);
            let state = crate::IteratorState::Array {
                array: *arr,
                index: 0,
                origin: crate::BuiltinIteratorOrigin::Array,
            };
            let handle = ctx
                .alloc_iterator_state(state, &[&arr_value], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name: "Iterator.from",
                    reason: "iterator allocation failed".to_string(),
                })?;
            Ok(Value::Iterator(handle))
        }
        Value::Generator(g) => {
            let state = crate::IteratorState::Generator { handle: *g };
            let handle = ctx
                .alloc_iterator_state(state, &[&input], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name: "Iterator.from",
                    reason: "iterator allocation failed".to_string(),
                })?;
            Ok(Value::Iterator(handle))
        }
        // §27.1.4.1 step 1 — `iterable` may also be an Object with
        // `@@iterator`; look up the method, call it, and wrap the
        // resulting iterator object in `IteratorState::User`. When
        // the receiver already exposes `.next` directly (the
        // already-iterator path) we wrap it as-is so subsequent
        // `IteratorNext` invocations drive it through the user
        // dispatcher.
        Value::Object(_)
        | Value::Set(_)
        | Value::Map(_)
        | Value::Function { .. }
        | Value::Closure(_)
        | Value::NativeFunction(_)
        | Value::BoundFunction(_)
        | Value::ClassConstructor(_)
        | Value::Proxy(_) => {
            let iterator_sym = ctx
                .cx
                .interp
                .well_known_symbols()
                .get(crate::symbol::WellKnown::Iterator);
            let (interp, exec_ctx) = ctx.interp_mut_and_context();
            let exec_ctx = exec_ctx.ok_or_else(|| crate::NativeError::TypeError {
                name: "Iterator.from",
                reason: "missing execution context".to_string(),
            })?;
            let key = crate::VmPropertyKey::Symbol(iterator_sym);
            let outcome = interp
                .ordinary_get_value(&exec_ctx, input.clone(), input.clone(), &key, 0)
                .map_err(|e| crate::NativeError::TypeError {
                    name: "Iterator.from",
                    reason: e.to_string(),
                })?;
            let iter_method = match outcome {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                    interp
                        .run_callable_sync(&exec_ctx, &getter, input.clone(), args)
                        .map_err(|e| crate::NativeError::TypeError {
                            name: "Iterator.from",
                            reason: e.to_string(),
                        })?
                }
            };
            let iter_value = if matches!(iter_method, Value::Undefined | Value::Null) {
                input.clone()
            } else if interp.is_callable_runtime(&iter_method) {
                let args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::new();
                interp
                    .run_callable_sync(&exec_ctx, &iter_method, input.clone(), args)
                    .map_err(|e| crate::NativeError::TypeError {
                        name: "Iterator.from",
                        reason: e.to_string(),
                    })?
            } else {
                return Err(crate::NativeError::TypeError {
                    name: "Iterator.from",
                    reason: "argument is not iterable".to_string(),
                });
            };
            if let Value::Iterator(_) = &iter_value {
                return Ok(iter_value);
            }
            let state = crate::IteratorState::User {
                iterator: iter_value.clone(),
            };
            let handle = ctx
                .alloc_iterator_state(state, &[&iter_value], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name: "Iterator.from",
                    reason: "iterator allocation failed".to_string(),
                })?;
            Ok(Value::Iterator(handle))
        }
        Value::String(s) => {
            let state = crate::IteratorState::String {
                string: s.clone(),
                index: 0,
            };
            let handle = ctx
                .alloc_iterator_state(state, &[&input], &[])
                .map_err(|_| crate::NativeError::TypeError {
                    name: "Iterator.from",
                    reason: "iterator allocation failed".to_string(),
                })?;
            Ok(Value::Iterator(handle))
        }
        _ => Err(crate::NativeError::TypeError {
            name: "Iterator.from",
            reason: "argument is not iterable".to_string(),
        }),
    }
}

fn define_global(global: JsObject, heap: &mut otter_gc::GcHeap, name: &'static str, value: Value) {
    let descriptor = PropertyDescriptor::data(
        value,
        Attr::global_binding().writable,
        Attr::global_binding().enumerable,
        Attr::global_binding().configurable,
    );
    let _ = object::define_own_property(global, heap, name, descriptor);
}

fn namespace_native_function_count(spec: &NamespaceSpec) -> usize {
    spec.methods
        .len()
        .saturating_add(
            spec.accessors
                .iter()
                .filter(|accessor| accessor.get.is_some())
                .count(),
        )
        .saturating_add(
            spec.accessors
                .iter()
                .filter(|accessor| accessor.set.is_some())
                .count(),
        )
}

fn record_installed_entry(telemetry: &mut BootstrapTelemetry, entry: &BootstrapEntry) {
    match entry.name {
        "Array" => {
            telemetry.entries_installed = telemetry.entries_installed.saturating_add(1);
            telemetry.objects_installed = telemetry.objects_installed.saturating_add(2);
            telemetry.prototype_objects_installed =
                telemetry.prototype_objects_installed.saturating_add(1);
            telemetry.native_functions_installed = telemetry
                .native_functions_installed
                .saturating_add(array_prototype::ARRAY_PROTOTYPE_METHODS.len());
        }
        "JSON" => telemetry.record_namespace(&json::JSON_SPEC),
        "Object" => telemetry.record_namespace(&object_statics::OBJECT_SPEC),
        "Math" => telemetry.record_namespace(&math::MATH_SPEC),
        "Atomics" => telemetry.record_namespace(&atomics::ATOMICS_SPEC),
        "Reflect" => telemetry.record_namespace(&reflect::REFLECT_SPEC),
        "Function" => {
            telemetry.entries_installed = telemetry.entries_installed.saturating_add(1);
            telemetry.objects_installed = telemetry.objects_installed.saturating_add(2);
            telemetry.prototype_objects_installed =
                telemetry.prototype_objects_installed.saturating_add(1);
            telemetry.native_functions_installed = telemetry
                .native_functions_installed
                .saturating_add(function_prototype::FUNCTION_PROTOTYPE_METHODS.len());
        }
        "Number" => {
            telemetry.entries_installed = telemetry.entries_installed.saturating_add(1);
            telemetry.objects_installed = telemetry.objects_installed.saturating_add(2);
            telemetry.prototype_objects_installed =
                telemetry.prototype_objects_installed.saturating_add(1);
            telemetry.native_functions_installed = telemetry
                .native_functions_installed
                .saturating_add(crate::number::prototype::NUMBER_PROTOTYPE_METHODS.len());
        }
        "console" => telemetry.record_namespace(&console::CONSOLE_SPEC),
        _ => telemetry.record_placeholder(),
    }
}

fn duplicate_name_count() -> usize {
    let mut names: Vec<&str> = BOOTSTRAP_ENTRIES.iter().map(|entry| entry.name).collect();
    names.sort_unstable();
    names.windows(2).filter(|pair| pair[0] == pair[1]).count()
}

fn allocation_snapshot(heap: &mut otter_gc::GcHeap) -> AllocationSnapshot {
    let stats = heap.gc_stats();
    let allocations = stats.by_type.iter().map(|row| row.alloc_count_total).sum();
    AllocationSnapshot {
        allocations,
        live_bytes: stats.live_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NativeFunction;

    #[test]
    fn registry_order_is_deterministic_and_unique() {
        let names: Vec<&str> = BOOTSTRAP_ENTRIES.iter().map(|entry| entry.name).collect();
        assert_eq!(names.first(), Some(&"Object"));
        assert_eq!(
            names.iter().position(|n| *n == "Array"),
            Some(1),
            "Array must install after Object so its [[Prototype]] can resolve"
        );
        assert_eq!(names.last(), Some(&"setTimeout"));
        assert!(
            names.contains(&"console"),
            "console entry must remain installed"
        );

        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn default_bootstrap_telemetry_matches_startup_ratchet() {
        // Bumped over time as the bootstrap installer family
        // grows: Map/Set/WeakMap/WeakSet (slice 9), Promise
        // (slice 10), RegExp (slice 11), WeakRef +
        // FinalizationRegistry (slice 12), `String.prototype`
        // method spec install pass (Iter 11),
        // `<Ctor>[Symbol.species]` accessors on Array / Map / Set /
        // RegExp / ArrayBuffer / SharedArrayBuffer plus
        // `Iterator.prototype.{next, return, throw}` installers.
        // Each ctor installs a `[[Construct]]` slot plus a prototype
        // with several native methods and (for some) accessors.
        const MAX_DEFAULT_GC_ALLOCATIONS: u64 = 1100;
        const MAX_DEFAULT_GC_ALLOCATED_BYTES: usize = 488 * 1024;

        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let mut telemetry = BootstrapTelemetry::default();
        let global =
            build_global_this_with_telemetry(&mut heap, BootstrapFeatures::all(), &mut telemetry)
                .expect("global");

        assert!(object::get(global, &heap, "Math").is_some());
        assert!(object::get(global, &heap, "Reflect").is_some());
        assert_eq!(telemetry.entries_considered(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.entries_installed(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.entries_skipped(), 0);
        assert_eq!(telemetry.duplicate_name_checks(), BOOTSTRAP_ENTRIES.len());
        assert_eq!(telemetry.duplicate_names_found(), 0);
        assert_eq!(telemetry.strings_interned(), 0);
        assert_eq!(telemetry.namespace_objects_installed(), 6);
        // 103 baseline + Object.is / Object.getPrototypeOf /
        // Object.setPrototypeOf (3) — all installed through the
        // `OBJECT_SPEC` namespace spec and therefore counted in
        // `native_functions_installed`. Date's static / prototype
        // methods are still installed via `ObjectBuilder::method_from_spec`
        // and don't bump the namespace counter. Array entry uses an
        // ad-hoc `record_installed_entry` branch that adds
        // `ARRAY_PROTOTYPE_METHODS.len()`, which grew by 3 with the
        // `copyWithin` / `toReversed` / `with` additions. Number
        // contributes `toLocaleString` as an own prototype builtin.
        assert_eq!(
            telemetry.native_functions_installed(),
            129 + reflect::REFLECT_SPEC.methods.len(),
        );
        assert!(
            telemetry.gc_allocations() <= MAX_DEFAULT_GC_ALLOCATIONS,
            "gc_allocations={} max={}",
            telemetry.gc_allocations(),
            MAX_DEFAULT_GC_ALLOCATIONS
        );
        assert!(
            telemetry.gc_allocated_bytes() <= MAX_DEFAULT_GC_ALLOCATED_BYTES,
            "gc_allocated_bytes={} max={}",
            telemetry.gc_allocated_bytes(),
            MAX_DEFAULT_GC_ALLOCATED_BYTES
        );
        assert_eq!(telemetry.phases().len(), BOOTSTRAP_ENTRIES.len() + 1);
    }

    #[test]
    fn feature_gates_skip_console() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let global = build_global_this_with_features(
            &mut heap,
            BootstrapFeatures::all().without(BootstrapFeatures::CONSOLE),
        )
        .expect("global");
        assert!(object::get(global, &heap, "Math").is_some());
        assert!(object::get(global, &heap, "console").is_none());
    }

    #[test]
    fn math_installs_with_static_native_methods_and_attrs() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let global = build_global_this(&mut heap).expect("global");
        let Value::Object(math) = object::get(global, &heap, "Math").expect("Math") else {
            panic!("Math should be an object")
        };

        let pi = object::get_own_descriptor(math, &heap, "PI").expect("PI");
        assert!(!pi.writable());
        assert!(!pi.enumerable());
        assert!(!pi.configurable());

        let Value::NativeFunction(abs) = object::get(math, &heap, "abs").expect("abs") else {
            panic!("Math.abs should be native")
        };
        assert!(NativeFunction::is_static_call(&abs, &heap));
        assert_eq!(abs.length(&heap), 1);
    }
}
