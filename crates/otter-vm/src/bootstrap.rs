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

use crate::js_surface::{Attr, JsSurfaceError, NamespaceBuilder, NamespaceSpec, ObjectBuilder};
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{
    Value, array_prototype, array_statics, atomics, console, function_prototype, json, math,
    object_statics, reflect,
};

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
    BootstrapEntry {
        name: object_statics::OBJECT_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_object,
    },
    BootstrapEntry {
        name: "Array",
        feature: BootstrapFeatures::CORE,
        install: install_array,
    },
    BootstrapEntry {
        name: json::JSON_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_json,
    },
    BootstrapEntry {
        name: "String",
        feature: BootstrapFeatures::CORE,
        install: install_string,
    },
    BootstrapEntry {
        name: "Number",
        feature: BootstrapFeatures::CORE,
        install: install_number,
    },
    BootstrapEntry {
        name: "Boolean",
        feature: BootstrapFeatures::CORE,
        install: install_boolean,
    },
    BootstrapEntry {
        name: "BigInt",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_bigint::install_bigint,
    },
    BootstrapEntry {
        name: "Symbol",
        feature: BootstrapFeatures::CORE,
        install: install_symbol,
    },
    BootstrapEntry {
        name: math::MATH_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_math,
    },
    BootstrapEntry {
        name: "Date",
        feature: BootstrapFeatures::CORE,
        install: install_date,
    },
    BootstrapEntry {
        name: "RegExp",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_regexp::install_regexp,
    },
    BootstrapEntry {
        name: "Map",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_collections::install_map,
    },
    BootstrapEntry {
        name: "Set",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_collections::install_set,
    },
    BootstrapEntry {
        name: "WeakMap",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_collections::install_weak_map,
    },
    BootstrapEntry {
        name: "WeakSet",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_collections::install_weak_set,
    },
    BootstrapEntry {
        name: "WeakRef",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_weak_refs::install_weak_ref,
    },
    BootstrapEntry {
        name: "Promise",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_promise::install_promise,
    },
    BootstrapEntry {
        name: "Proxy",
        feature: BootstrapFeatures::CORE,
        install: install_proxy,
    },
    BootstrapEntry {
        name: reflect::REFLECT_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_reflect,
    },
    BootstrapEntry {
        name: "Function",
        feature: BootstrapFeatures::CORE,
        install: install_function,
    },
    BootstrapEntry {
        name: "ArrayBuffer",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_array_buffer::install_array_buffer,
    },
    BootstrapEntry {
        name: "SharedArrayBuffer",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_array_buffer::install_shared_array_buffer,
    },
    BootstrapEntry {
        name: "DataView",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_data_view::install_data_view,
    },
    typed_array_entry("Int8Array"),
    typed_array_entry("Uint8Array"),
    typed_array_entry("Uint8ClampedArray"),
    typed_array_entry("Int16Array"),
    typed_array_entry("Uint16Array"),
    typed_array_entry("Int32Array"),
    typed_array_entry("Uint32Array"),
    typed_array_entry("Float32Array"),
    typed_array_entry("Float64Array"),
    typed_array_entry("BigInt64Array"),
    typed_array_entry("BigUint64Array"),
    BootstrapEntry {
        name: atomics::ATOMICS_SPEC.name,
        feature: BootstrapFeatures::CORE,
        install: install_atomics,
    },
    placeholder("Intl"),
    placeholder("Temporal"),
    placeholder("AggregateError"),
    BootstrapEntry {
        name: "FinalizationRegistry",
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_weak_refs::install_finalization_registry,
    },
    placeholder("Iterator"),
    BootstrapEntry {
        name: console::CONSOLE_SPEC.name,
        feature: BootstrapFeatures::CONSOLE,
        install: install_console,
    },
    BootstrapEntry {
        name: "setTimeout",
        feature: BootstrapFeatures::CORE,
        install: install_timer_globals,
    },
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

    let global = object::alloc_object(heap)?;
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

const fn placeholder(name: &'static str) -> BootstrapEntry {
    BootstrapEntry {
        name,
        feature: BootstrapFeatures::CORE,
        install: install_placeholder,
    }
}

/// Build a bootstrap entry for one of the 11 concrete TypedArray
/// constructors. Routes to
/// [`crate::bootstrap_typed_array::install_typed_array_entry`].
const fn typed_array_entry(name: &'static str) -> BootstrapEntry {
    BootstrapEntry {
        name,
        feature: BootstrapFeatures::CORE,
        install: crate::bootstrap_typed_array::install_typed_array_entry,
    }
}

fn install_placeholder(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let placeholder = object::alloc_object(heap)?;
    let proto = object::alloc_object(heap)?;
    object::set(placeholder, heap, "prototype", Value::Object(proto));
    define_global(global, heap, entry.name, Value::Object(placeholder));
    Ok(())
}

fn install_proxy(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
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
                | Value::Date(_)
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
        let proxy_handle = proxy.clone();
        let revoke = crate::native_function::native_value_unchecked(
            ctx.heap_mut(),
            "revoke",
            move |_, _, _| {
                proxy_handle.revoke();
                Ok(Value::Undefined)
            },
        )
        .map_err(|_| NativeError::TypeError {
            name: "Proxy.revocable",
            reason: "out of memory while creating revoke function".to_string(),
        })?;
        let obj = object::alloc_object(ctx.heap_mut()).map_err(|_| NativeError::TypeError {
            name: "Proxy.revocable",
            reason: "out of memory while creating result object".to_string(),
        })?;
        object::set(obj, ctx.heap_mut(), "proxy", Value::Proxy(proxy));
        object::set(obj, ctx.heap_mut(), "revoke", revoke);
        Ok(Value::Object(obj))
    }

    let proxy_ctor = NativeFunction::new_constructor_static(heap, "Proxy", 2, proxy_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let revocable = NativeFunction::new_static(heap, "revocable", 2, proxy_revocable_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let revocable_desc =
        PropertyDescriptor::data(Value::NativeFunction(revocable), true, false, true);
    let string_heap = crate::string::StringHeap::default();
    if !proxy_ctor.define_own_property(heap, &string_heap, "revocable", revocable_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("revocable"));
    }
    define_global(global, heap, entry.name, Value::NativeFunction(proxy_ctor));
    Ok(())
}

// §20.4.1 The Symbol Constructor — ordinary function callable as
// `Symbol(desc)`. Calling with `new` rejects per §20.4.1.1.
// Exposes every well-known symbol as an own data property
// (configurable=false, writable=false, enumerable=false per
// §20.4.2.*), plus `for` / `keyFor` methods and a `prototype` link.
// <https://tc39.es/ecma262/#sec-symbol-constructor>
fn install_symbol(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
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
                let rendered = crate::string::JsString::from_str(
                    &other.display_string(),
                    &string_heap,
                )
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
                let value = crate::string::JsString::from_str(&key, &string_heap).map_err(
                    |_| NativeError::TypeError {
                        name: "Symbol.keyFor",
                        reason: "out of memory".to_string(),
                    },
                )?;
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
    let symbol_ctor = NativeFunction::new_static(heap, "Symbol", 0, symbol_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;

    // §20.4.3 Symbol.prototype — ordinary object linked to %Object.prototype%.
    let prototype = object::alloc_object(heap)?;
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
        let mut builder = ObjectBuilder::from_object(heap, prototype);
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
        let getter = NativeFunction::new_static(
            heap,
            "get description",
            0,
            symbol_proto_description_get,
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let desc_desc = PropertyDescriptor::accessor(
            Some(Value::NativeFunction(getter)),
            None,
            false,
            true,
        );
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
    let symbol_for_fn = NativeFunction::new_static(heap, "for", 1, symbol_for_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let symbol_key_for_fn = NativeFunction::new_static(heap, "keyFor", 1, symbol_key_for_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let for_desc =
        PropertyDescriptor::data(Value::NativeFunction(symbol_for_fn), true, false, true);
    let key_for_desc = PropertyDescriptor::data(
        Value::NativeFunction(symbol_key_for_fn),
        true,
        false,
        true,
    );
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
    define_global(global, heap, entry.name, Value::NativeFunction(symbol_ctor));
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
    use crate::native_function::NativeFunction;
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
        crate::object::DescriptorKind::Data { value: Value::Object(p) } => Some(p),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let to_prim_fn = NativeFunction::new_static(
        heap,
        "[Symbol.toPrimitive]",
        1,
        symbol_proto_to_primitive,
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
    // §25.2.5 — `SharedArrayBuffer.prototype[@@toStringTag]`.
    crate::bootstrap_array_buffer::install_shared_array_buffer_well_knowns_post_bootstrap(
        heap,
        string_heap,
        global,
        well_known,
    )?;
    Ok(())
}

fn install_array(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
    use crate::{NativeCtx, NativeError};

    let array = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
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
        let mut builder = ObjectBuilder::from_object(heap, array);
        for method in array_statics::ARRAY_STATIC_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
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
        let arr =
            crate::array::alloc_array(ctx.heap_mut()).map_err(|_| NativeError::TypeError {
                name: "Array",
                reason: "out of memory while allocating array".to_string(),
            })?;
        if args.len() == 1
            && let Value::Number(n) = &args[0]
        {
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
                crate::array::set(arr, ctx.heap_mut(), last, Value::Hole).map_err(|_| {
                    NativeError::TypeError {
                        name: "Array",
                        reason: "out of memory while sizing array".to_string(),
                    }
                })?;
            }
            return Ok(Value::Array(arr));
        }
        for v in args {
            crate::array::push(arr, ctx.heap_mut(), v.clone()).map_err(|_| {
                NativeError::TypeError {
                    name: "Array",
                    reason: "out of memory while populating array".to_string(),
                }
            })?;
        }
        Ok(Value::Array(arr))
    }

    let ctor_native = NativeFunction::new_static(heap, "Array", 1, array_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    // Wire the callable+constructable bridge as an internal object
    // slot. This must not appear in JS own-property reflection.
    object::set_constructor_native(array, heap, Value::NativeFunction(ctor_native));

    define_global(global, heap, entry.name, Value::Object(array));
    Ok(())
}

fn install_number(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
    use crate::{NativeCall, NativeCtx, NativeError};

    // Number.prototype with all the formatter methods + the
    // hidden `[[NumberData]]` slot (= +0 per §21.1.3) so
    // `Number.prototype.toString()` recovers the value.
    let prototype = object::alloc_object(heap)?;
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
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
            crate::number::NumberValue::from_f64(crate::number::parse::to_number_value(&args[0]))
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

    let ctor_native = NativeFunction::new_static(heap, "Number", 1, number_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    // The `Number` global itself is a GC-managed JsObject. Both the
    // constants/static methods and the `prototype` link sit on it
    // as ordinary properties; the callable+constructable surface is
    // wired through the dispatch path's internal native-constructor
    // slot.
    let statics = object::alloc_object(heap)?;
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
    object::set_constructor_native(statics, heap, Value::NativeFunction(ctor_native));
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
        let mut builder = ObjectBuilder::from_object(heap, statics);
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
        let mut builder = ObjectBuilder::from_object(heap, statics);
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
    }

    let number_value = Value::Object(statics);
    // §21.1.3.1 `Number.prototype.constructor` points back at the
    // Number constructor.
    object::set(prototype, heap, "constructor", number_value.clone());
    define_global(global, heap, entry.name, number_value);
    Ok(())
}

fn install_string(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
    use crate::{NativeCtx, NativeError};

    let constructor = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(constructor, heap, Some(object_proto));
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    crate::object::set_string_data(
        prototype,
        heap,
        crate::string::JsString::from_str("", &crate::string::StringHeap::default())
            .map_err(|_| JsSurfaceError::OutOfMemory)?,
    );

    fn string_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let string_heap = ctx.interp_mut().string_heap_clone();
        let value = crate::string_dispatch::call(
            otter_bytecode::method_id::StringMethod::Construct,
            args,
            &string_heap,
        )
        .map_err(|err| NativeError::TypeError {
            name: "String",
            reason: err.to_string(),
        })?;
        if ctx.is_construct_call() {
            let Value::String(string) = value else {
                return Err(NativeError::TypeError {
                    name: "String",
                    reason: "constructor did not return a string primitive".to_string(),
                });
            };
            let this = ctx.this_value().clone();
            if let Value::Object(obj) = this {
                crate::object::set_string_data(obj, ctx.heap_mut(), string);
                Ok(Value::Object(obj))
            } else {
                Err(NativeError::TypeError {
                    name: "String",
                    reason: "expected object receiver in `new String(...)`".to_string(),
                })
            }
        } else {
            Ok(value)
        }
    }

    let ctor_native = NativeFunction::new_static(heap, "String", 1, string_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(constructor, heap, Value::NativeFunction(ctor_native));
    object::set(constructor, heap, "prototype", Value::Object(prototype));
    let string_value = Value::Object(constructor);
    object::set(prototype, heap, "constructor", string_value.clone());
    define_global(global, heap, entry.name, string_value);
    Ok(())
}

fn install_boolean(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
    use crate::{NativeCtx, NativeError};

    let prototype = object::alloc_object(heap)?;
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        for method in crate::boolean_prototype::BOOLEAN_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    crate::object::set_boolean_data(prototype, heap, false);
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }

    fn boolean_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let value = args.first().is_some_and(Value::to_boolean);
        if ctx.is_construct_call() {
            let this = ctx.this_value().clone();
            if let Value::Object(obj) = this {
                crate::object::set_boolean_data(obj, ctx.heap_mut(), value);
                Ok(Value::Object(obj))
            } else {
                Err(NativeError::TypeError {
                    name: "Boolean",
                    reason: "expected object receiver in `new Boolean(...)`".to_string(),
                })
            }
        } else {
            Ok(Value::Boolean(value))
        }
    }

    let ctor_native = NativeFunction::new_static(heap, "Boolean", 1, boolean_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let statics = object::alloc_object(heap)?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(statics, heap, Some(object_proto));
    }
    object::set_constructor_native(statics, heap, Value::NativeFunction(ctor_native));
    object::set(statics, heap, "prototype", Value::Object(prototype));
    let boolean_value = Value::Object(statics);
    object::set(prototype, heap, "constructor", boolean_value.clone());
    define_global(global, heap, entry.name, boolean_value);
    Ok(())
}

fn install_function(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
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
            .build_function_constructor(&context, args)
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

    let function = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    object::set_prototype(function, heap, Some(prototype));
    let ctor_native =
        NativeFunction::new_constructor_static(heap, "Function", 1, function_ctor_call)
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(function, heap, Value::NativeFunction(ctor_native));
    let prototype_call = NativeFunction::new_static(heap, "", 0, function_prototype_call)
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
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        for method in function_prototype::FUNCTION_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    function_prototype::install_restricted_accessors(heap, prototype)?;
    let constructor = PropertyDescriptor::data(Value::Object(function), true, false, true);
    let _ = object::define_own_property(prototype, heap, "constructor", constructor);
    define_global(global, heap, entry.name, Value::Object(function));
    Ok(())
}

fn install_math(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let namespace = NamespaceBuilder::from_spec(heap, &math::MATH_SPEC)
        .map_err(JsSurfaceError::from)?
        .build()?;
    define_global(global, heap, entry.name, Value::Object(namespace));
    Ok(())
}

fn install_json(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let namespace = NamespaceBuilder::from_spec(heap, &json::JSON_SPEC)?.build()?;
    define_global(global, heap, entry.name, Value::Object(namespace));
    Ok(())
}

fn install_object(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
    use crate::{NativeCtx, NativeError};

    fn object_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        match args.first() {
            None | Some(Value::Undefined | Value::Null) => {
                let obj =
                    object::alloc_object(ctx.heap_mut()).map_err(|_| NativeError::TypeError {
                        name: "Object",
                        reason: "object allocation failed".to_string(),
                    })?;
                Ok(Value::Object(obj))
            }
            Some(value) => Ok(value.clone()),
        }
    }

    let object = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
    let ctor_native = NativeFunction::new_static(heap, "Object", 1, object_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(object, heap, Value::NativeFunction(ctor_native));
    object::set(object, heap, "prototype", Value::Object(prototype));
    {
        let mut builder = ObjectBuilder::from_object(heap, object);
        for method in object_statics::OBJECT_SPEC.methods {
            builder.method_from_spec(method)?;
        }
    }
    {
        let mut builder = ObjectBuilder::from_object(heap, prototype);
        for method in object_statics::OBJECT_PROTOTYPE_METHODS {
            builder.method_from_spec(method)?;
        }
    }
    object::set(prototype, heap, "constructor", Value::Object(object));
    define_global(global, heap, entry.name, Value::Object(object));
    Ok(())
}

fn install_date(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    use crate::native_function::NativeFunction;
    use crate::{JsString, NativeCtx, NativeError};

    fn date_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        let date =
            crate::date::dispatch::call(otter_bytecode::method_id::DateMethod::Construct, args)
                .map_err(|err| NativeError::TypeError {
                    name: "Date",
                    reason: err.to_string(),
                })?;
        if ctx.is_construct_call() {
            return Ok(date);
        }
        let text = match &date {
            Value::Date(d) => {
                crate::date::to_iso_string(d.time()).unwrap_or_else(|| "Invalid Date".to_string())
            }
            other => other.display_string(),
        };
        let string_heap = ctx.interp_mut().string_heap_clone();
        let value =
            JsString::from_str(&text, &string_heap).map_err(|err| NativeError::TypeError {
                name: "Date",
                reason: err.to_string(),
            })?;
        Ok(Value::String(value))
    }

    let constructor = object::alloc_object(heap)?;
    let prototype = object::alloc_object(heap)?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(constructor, heap, Some(object_proto));
        object::set_prototype(prototype, heap, Some(object_proto));
    }
    let ctor_native = NativeFunction::new_static(heap, "Date", 7, date_ctor_call)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_constructor_native(constructor, heap, Value::NativeFunction(ctor_native));
    object::set(constructor, heap, "prototype", Value::Object(prototype));
    let date_value = Value::Object(constructor);
    object::set(prototype, heap, "constructor", date_value.clone());
    define_global(global, heap, entry.name, date_value);
    Ok(())
}

fn install_atomics(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let namespace = NamespaceBuilder::from_spec(heap, &atomics::ATOMICS_SPEC)?.build()?;
    define_global(global, heap, entry.name, Value::Object(namespace));
    Ok(())
}

// §28.1 Reflect — ordinary namespace object with own data properties
// for every spec method. Links the namespace prototype to
// `%Object.prototype%` so reflective inspection (`Object.getPrototypeOf
// (Reflect) === Object.prototype`) returns the spec-required value.
// <https://tc39.es/ecma262/#sec-reflect-object>
fn install_reflect(
    entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    let namespace = NamespaceBuilder::from_spec(heap, &reflect::REFLECT_SPEC)?.build()?;
    if let Some(Value::Object(object_ctor)) = object::get(global, heap, "Object")
        && let Some(Value::Object(object_proto)) = object::get(object_ctor, heap, "prototype")
    {
        object::set_prototype(namespace, heap, Some(object_proto));
    }
    define_global(global, heap, entry.name, Value::Object(namespace));
    Ok(())
}

fn install_console(
    _entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    console::install(global, heap)
}

fn install_timer_globals(
    _entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    crate::timers::install_timer_globals(global, heap)
}

pub(crate) fn define_global_value(
    global: JsObject,
    heap: &mut otter_gc::GcHeap,
    name: &'static str,
    value: Value,
) {
    define_global(global, heap, name, value);
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
        // FinalizationRegistry (slice 12). Each ctor installs a
        // `[[Construct]]` slot plus a prototype with several
        // native methods and (for some) accessors.
        const MAX_DEFAULT_GC_ALLOCATIONS: u64 = 640;
        const MAX_DEFAULT_GC_ALLOCATED_BYTES: usize = 280 * 1024;

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
        assert_eq!(
            telemetry.native_functions_installed(),
            101 + reflect::REFLECT_SPEC.methods.len(),
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
