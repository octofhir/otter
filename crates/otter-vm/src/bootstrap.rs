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
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{
    Value, array_prototype, array_statics, atomics, console, function_prototype, json, math,
    object_statics, reflect,
};

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
                | Value::Closure { .. }
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

    fn proxy_handler_arg(args: &[Value]) -> Result<JsObject, NativeError> {
        match args.get(1) {
            Some(Value::Object(handler)) => Ok(*handler),
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
        Ok(Value::Proxy(crate::proxy::JsProxy::new(target, handler)))
    }

    fn proxy_revocable_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let target = proxy_target_arg(args)?;
        let handler = proxy_handler_arg(args)?;
        let proxy = crate::proxy::JsProxy::new(target, handler);
        let proxy_value = Value::Proxy(proxy.clone());
        let revoke = ctx
            .native_value_with_captures(
                "revoke",
                smallvec::smallvec![proxy_value.clone()],
                &[],
                &[args],
                move |_, _, captures| {
                    if let Some(Value::Proxy(proxy)) = captures.first() {
                        proxy.revoke();
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
        object::set(obj, ctx.heap_mut(), "proxy", Value::Proxy(proxy));
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
        let description = match args.first() {
            None | Some(Value::Undefined) => None,
            Some(Value::Symbol(_)) => {
                return Err(NativeError::TypeError {
                    name: "Symbol",
                    reason: "Cannot convert a Symbol value to a string".to_string(),
                });
            }
            Some(other) => {
                let string_heap = ctx.interp_mut().string_heap_clone();
                let rendered =
                    crate::string::JsString::from_str(&other.display_string(), &string_heap)
                        .map_err(|_| NativeError::TypeError {
                            name: "Symbol",
                            reason: "out of memory".to_string(),
                        })?;
                Some(rendered)
            }
        };
        Ok(Value::Symbol(crate::symbol::JsSymbol::new(description)))
    }

    fn symbol_for_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let key = match args.first() {
            None | Some(Value::Undefined) => "undefined".to_string(),
            Some(Value::Null) => "null".to_string(),
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(Value::Symbol(_)) => {
                return Err(NativeError::TypeError {
                    name: "Symbol.for",
                    reason: "Cannot convert a Symbol value to a string".to_string(),
                });
            }
            Some(other) => other.display_string(),
        };
        let string_heap = ctx.interp_mut().string_heap_clone();
        let sym = ctx
            .interp_mut()
            .symbol_registry()
            .for_key(&key, &string_heap)
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
        match ctx.this_value() {
            Value::Symbol(sym) => Ok(Value::Symbol(sym.clone())),
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
        match ctx.this_value() {
            Value::Symbol(sym) => Ok(Value::Symbol(sym.clone())),
            _ => Err(NativeError::TypeError {
                name: "Symbol.prototype[@@toPrimitive]",
                reason: "this is not a Symbol".to_string(),
            }),
        }
    }

    // The Symbol constructor itself is a callable NativeFunction.
    let global_root = Value::Object(global);
    let symbol_ctor =
        native_static_with_value_roots(heap, "Symbol", 0, symbol_ctor_call, &[&global_root])
            .map_err(|_| JsSurfaceError::OutOfMemory)?;

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
        match ctx.this_value() {
            Value::Symbol(sym) => match sym.description() {
                Some(s) => Ok(Value::String(s.clone())),
                None => Ok(Value::Undefined),
            },
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
    object::set(
        prototype,
        heap,
        "constructor",
        Value::NativeFunction(symbol_ctor),
    );
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
        match ctx.this_value() {
            Value::Symbol(sym) => Ok(Value::Symbol(sym.clone())),
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
    object::set_symbol(
        prototype,
        heap,
        to_primitive_sym,
        Value::NativeFunction(to_prim_fn),
    );

    // §22.2 / §25.1 / §25.4 — install `@@toStringTag` on standard
    // namespace objects so `Object.prototype.toString.call(NS)`
    // returns the spec-required `"[object <NS>]"` form.
    let to_string_tag_sym = well_known.get(WellKnown::ToStringTag);
    for ns_name in ["Math", "JSON", "Reflect", "Atomics"] {
        if let Some(Value::Object(ns)) = object::get(global, heap, ns_name) {
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
        heap,
        string_heap,
        global,
        well_known,
    )?;
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
    // §25.2.5 — `SharedArrayBuffer.prototype[@@toStringTag]`.
    crate::bootstrap_array_buffer::install_shared_array_buffer_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
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
    object::set(array, heap, "prototype", Value::Object(prototype));
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
            return Ok(Value::Array(arr));
        }
        unreachable!("non-numeric Array(...) arguments returned above")
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
            // §21.1.1.1 step 5 — `Number(bigint)` does NOT throw; it
            // converts to the nearest f64 representation. Generic
            // §7.1.4 ToNumber on BigInt throws TypeError (see
            // `language/expressions/unary-plus/bigint-throws.js`),
            // so the constructor diverges here from `Op::ToNumber`.
            // <https://tc39.es/ecma262/#sec-number-constructor-number-value>
            if let Value::BigInt(b) = &args[0] {
                let f = b.to_decimal_string().parse::<f64>().unwrap_or(f64::NAN);
                crate::number::NumberValue::from_f64(f)
            } else {
                crate::number::NumberValue::from_f64(crate::number::parse::to_number_value(
                    &args[0],
                ))
            }
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
    // constructor object (per §21.1.2.5). Spec posture is
    // `[[Writable]]: false, [[Enumerable]]: false,
    // [[Configurable]]: false`; ordinary `set` matches the first
    // and third but installs as enumerable — the descriptor surface
    // is tightened separately when the rest of the spec descriptors
    // get audited.
    object::set(statics, heap, "prototype", Value::Object(prototype));

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
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = match args.first() {
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(other) => other.display_string(),
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
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = match args.first() {
            Some(Value::String(s)) => s.to_lossy_string(),
            Some(other) => other.display_string(),
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

        let global_methods: &[(&'static str, u8, crate::native_function::NativeFastFn)] = &[
            ("parseInt", 2, number_parse_int_native),
            ("parseFloat", 1, number_parse_float_native),
            ("isNaN", 1, number_is_nan_native),
            ("isFinite", 1, number_is_finite_native),
            ("encodeURI", 1, global_encode_uri),
            ("encodeURIComponent", 1, global_encode_uri_component),
            ("decodeURI", 1, global_decode_uri),
            ("decodeURIComponent", 1, global_decode_uri_component),
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
    object::set(prototype, heap, "constructor", number_value.clone());
    define_global(global, heap, "Number", number_value);
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
        let (interp, context) = ctx.interp_mut_and_context();
        let Some(context) = context else {
            return Err(NativeError::TypeError {
                name: "Function",
                reason: "missing execution context for Function constructor".to_string(),
            });
        };
        interp
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
            })
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
    /// - With no args / null / undefined: `OrdinaryObjectCreate(%Object.prototype%)`.
    ///   The new object's `[[Prototype]]` is wired to the realm's
    ///   `Object.prototype` so the inherited `toString` / `valueOf` /
    ///   `hasOwnProperty` etc. resolve.
    /// - With an object-typed arg: ToObject is the identity, so we
    ///   return the value untouched.
    /// - With a primitive arg (Boolean / Number / String / Symbol /
    ///   BigInt): the spec calls ToObject which produces a fresh
    ///   wrapper. We currently return the primitive unchanged here —
    ///   wrappers are produced by the dedicated constructors
    ///   (`new Number(x)` etc.). Primitive operands are uncommon for
    ///   the bare-call form and don't crash; `valueOf()` / equality
    ///   continue to work on the primitive.
    fn object_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
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
            Some(value) => Ok(value.clone()),
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
    object::set(object, heap, "prototype", Value::Object(prototype));
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
    object::set(prototype, heap, "constructor", Value::Object(object));
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
    object::set(constructor, heap, "prototype", Value::Object(prototype));

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
    object::set(prototype, heap, "constructor", date_value.clone());
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

/// Placeholder `BuiltinIntrinsic` for `Iterator` (the ES2025
/// constructor; iterator-helpers proposal not yet implemented).
pub struct IteratorIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for IteratorIntrinsic {
    const NAME: &'static str = "Iterator";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_placeholder(Self::NAME, heap, global)
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
        // method spec install pass (Iter 11). Each ctor installs a
        // `[[Construct]]` slot plus a prototype with several native
        // methods and (for some) accessors.
        const MAX_DEFAULT_GC_ALLOCATIONS: u64 = 915;
        const MAX_DEFAULT_GC_ALLOCATED_BYTES: usize = 400 * 1024;

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
        // `copyWithin` / `toReversed` / `with` additions.
        assert_eq!(
            telemetry.native_functions_installed(),
            127 + reflect::REFLECT_SPEC.methods.len(),
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
