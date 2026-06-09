//! `%Symbol%` constructor installer + post-bootstrap well-known wiring.
//!
//! Routes through `couch!` with a real `[[Construct]]` slot because
//! §20.4.1 keeps `Symbol` constructor-branded while making construct
//! calls throw. Static `for` / `keyFor`, prototype `toString` /
//! `valueOf`, and the `Symbol.prototype.description` getter ride the
//! declarative rows. All well-known symbol own properties
//! (`Symbol.iterator`,
//! `Symbol.toPrimitive`, …), the `Symbol.prototype[@@toPrimitive]`
//! method, and the cross-class `@@toStringTag` / `@@iterator` / species
//! fixups that depend on the per-realm `WellKnownSymbols` singleton
//! live in [`install_symbol_well_knowns_post_bootstrap`].
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-symbol-constructor>

use crate::Value;
use crate::bootstrap::{
    install_iterator_well_knowns_post_bootstrap, native_static_with_value_roots,
};
use crate::js_surface::JsSurfaceError;
use crate::object::{self, JsObject, PropertyDescriptor};
use crate::{NativeCtx, NativeError};

otter_macros::couch! {
    name = "Symbol",
    feature = CORE,
    constructor = (length = 0, call = symbol_ctor_call),
    statics = {
        "for"    / 1 => symbol_for_call,
        "keyFor" / 1 => symbol_key_for_call,
    },
    prototype = {
        method_specs = [crate::symbol_prototype::SYMBOL_PROTOTYPE_METHODS],
        accessors = [
            ("description", get = symbol_proto_description_get),
        ],
    },
}

// ---------------------------------------------------------------
// Constructor body
// ---------------------------------------------------------------

fn symbol_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if ctx.is_construct_call() {
        return Err(NativeError::TypeError {
            name: "Symbol",
            reason: "Symbol is not a constructor".to_string(),
        });
    }
    let description =
        match args.first() {
            None => None,
            Some(v) if v.is_undefined() => None,
            Some(other) => {
                let context =
                    ctx.execution_context()
                        .cloned()
                        .ok_or_else(|| NativeError::TypeError {
                            name: "Symbol",
                            reason: "missing execution context".to_string(),
                        })?;
                let coerced =
                    ctx.cx
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
                let rendered = crate::string::JsString::from_str(&coerced, ctx.heap_mut())
                    .map_err(|_| NativeError::TypeError {
                        name: "Symbol",
                        reason: "out of memory".to_string(),
                    })?;
                Some(rendered)
            }
        };
    let sym = crate::symbol::JsSymbol::new(ctx.interp_mut().gc_heap_mut(), description).map_err(
        |_| NativeError::TypeError {
            name: "Symbol",
            reason: "out of memory".to_string(),
        },
    )?;
    Ok(Value::symbol(sym))
}

// ---------------------------------------------------------------
// Statics
// ---------------------------------------------------------------

fn symbol_for_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let key = match args.first() {
        None => "undefined".to_string(),
        Some(v) if v.is_undefined() => "undefined".to_string(),
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
    Ok(Value::symbol(sym))
}

fn symbol_key_for_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(sym) = args.first().and_then(|v| v.as_symbol(ctx.heap())) else {
        return Err(NativeError::TypeError {
            name: "Symbol.keyFor",
            reason: "argument must be a symbol".to_string(),
        });
    };
    let key = ctx.interp_mut().symbol_registry().key_for(sym);
    match key {
        Some(key) => {
            let value = crate::string::JsString::from_str(&key, ctx.heap_mut()).map_err(|_| {
                NativeError::TypeError {
                    name: "Symbol.keyFor",
                    reason: "out of memory".to_string(),
                }
            })?;
            Ok(Value::string(value))
        }
        None => Ok(Value::undefined()),
    }
}

fn symbol_proto_description_get(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let this = *ctx.this_value();
    if let Some(sym) = this.as_symbol(ctx.heap()) {
        return match sym.description() {
            Some(s) => Ok(Value::string(*s)),
            None => Ok(Value::undefined()),
        };
    }
    if let Some(obj) = this.as_object() {
        let heap = ctx.interp_mut().gc_heap();
        return match crate::object::symbol_data(obj, heap) {
            Some(sym) => match sym.description() {
                Some(s) => Ok(Value::string(*s)),
                None => Ok(Value::undefined()),
            },
            None => Err(NativeError::TypeError {
                name: "get Symbol.prototype.description",
                reason: "this is not a Symbol".to_string(),
            }),
        };
    }
    Err(NativeError::TypeError {
        name: "get Symbol.prototype.description",
        reason: "this is not a Symbol".to_string(),
    })
}

// ---------------------------------------------------------------
// Post-bootstrap well-known wiring (depends on per-realm
// WellKnownSymbols table — runs after the table is materialised).
// ---------------------------------------------------------------

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
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    fn symbol_proto_to_primitive(
        ctx: &mut crate::NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, crate::NativeError> {
        let this = *ctx.this_value();
        if let Some(sym) = this.as_symbol(ctx.heap()) {
            return Ok(Value::symbol(sym));
        }
        if let Some(obj) = this.as_object() {
            let heap = ctx.interp_mut().gc_heap();
            return crate::object::symbol_data(obj, heap)
                .map(Value::symbol)
                .ok_or_else(|| crate::NativeError::TypeError {
                    name: "Symbol.prototype[@@toPrimitive]",
                    reason: "this is not a Symbol".to_string(),
                });
        }
        Err(crate::NativeError::TypeError {
            name: "Symbol.prototype[@@toPrimitive]",
            reason: "this is not a Symbol".to_string(),
        })
    }

    let Some(symbol_ctor_value) = object::get(global, heap, "Symbol") else {
        return Ok(());
    };
    let Some(symbol_ctor) = symbol_ctor_value.as_native_function() else {
        return Ok(());
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
        let desc = PropertyDescriptor::data(Value::symbol(sym), false, false, false);
        if !symbol_ctor.define_own_property(heap, name, desc) {
            return Err(JsSurfaceError::DefinePropertyFailed("well-known symbol"));
        }
    }

    // Symbol.prototype[@@toPrimitive] — ECMA-262 §20.4.3.5.
    let proto_desc = symbol_ctor
        .own_property_descriptor(heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match proto_desc.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data { value } => value.as_object(),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    let symbol_ctor_root = Value::native_function(symbol_ctor);
    let prototype_root = Value::object(prototype);
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
        PropertyDescriptor::data(Value::native_function(to_prim_fn), false, false, true);
    if !object::define_own_symbol_property(prototype, heap, to_primitive_sym, to_prim_desc) {
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
    let object_proto = object::get(global, heap, "Object").and_then(|v| {
        if let Some(ctor) = v.as_native_function() {
            ctor.own_property_descriptor(heap, "prototype")
                .ok()
                .flatten()
                .and_then(|d| match d.kind {
                    crate::object::DescriptorKind::Data { value } => value.as_object(),
                    _ => None,
                })
        } else if let Some(ctor) = v.as_object() {
            object::get(ctor, heap, "prototype").and_then(|v| v.as_object())
        } else {
            None
        }
    });
    for ns_name in ["Math", "JSON", "Reflect", "Atomics"] {
        if let Some(ns) = object::get(global, heap, ns_name).and_then(|v| v.as_object()) {
            if let Some(proto) = object_proto {
                object::set_prototype(ns, heap, Some(proto));
            }
            let tag = crate::string::JsString::from_str(ns_name, heap)
                .map_err(|_| JsSurfaceError::OutOfMemory)?;
            object::define_own_symbol_property_partial(
                ns,
                heap,
                to_string_tag_sym,
                crate::object::PartialPropertyDescriptor {
                    value: Some(Value::string(tag)),
                    writable: Some(false),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );
        }
    }
    // §20.4.3.5 — install `Symbol.prototype[@@toStringTag] = "Symbol"`.
    let symbol_tag = crate::string::JsString::from_str("Symbol", heap)
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        to_string_tag_sym,
        crate::object::PartialPropertyDescriptor {
            value: Some(Value::string(symbol_tag)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    crate::bootstrap_collections::install_collection_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    crate::bootstrap_promise::install_promise_well_knowns_post_bootstrap(heap, global, well_known)?;
    crate::bootstrap_weak_refs::install_weak_well_knowns_post_bootstrap(heap, global, well_known)?;
    crate::bootstrap_bigint::install_bigint_well_knowns_post_bootstrap(heap, global, well_known)?;
    crate::bootstrap_data_view::install_data_view_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    crate::temporal::intrinsic::install_temporal_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    crate::bootstrap_typed_array::install_typed_array_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    crate::array_prototype::install_array_well_knowns_post_bootstrap(heap, global, well_known)?;
    install_iterator_well_knowns_post_bootstrap(heap, global, well_known)?;
    crate::bootstrap_array_buffer::install_array_buffer_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    crate::date::well_known::install_date_well_knowns_post_bootstrap(heap, global, well_known)?;
    crate::install_string_iterator_post_bootstrap(heap, global, well_known)?;
    crate::bootstrap_regexp::install_regexp_well_knowns_post_bootstrap(heap, global, well_known)?;
    crate::bootstrap_array_buffer::install_shared_array_buffer_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    for ctor_name in [
        "Array",
        "Map",
        "Set",
        "RegExp",
        "ArrayBuffer",
        "SharedArrayBuffer",
        // §23.2.2.4 `get %TypedArray% [ @@species ]` lives on the
        // abstract constructor (hidden global slot); every per-kind
        // `Int8Array` … inherits it through its constructor
        // [[Prototype]] chain, so subclasses observe a working
        // SpeciesConstructor.
        "@@%TypedArray%",
    ] {
        install_constructor_species_accessor(heap, global, well_known, ctor_name)?;
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
    global: JsObject,
    well_known: &crate::symbol::WellKnownSymbols,
    ctor_name: &'static str,
) -> Result<(), JsSurfaceError> {
    use crate::symbol::WellKnown;

    fn species_get(
        ctx: &mut crate::NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, crate::NativeError> {
        Ok(*ctx.this_value())
    }

    let Some(ctor_value) = object::get(global, heap, ctor_name) else {
        return Ok(());
    };
    let global_root = Value::object(global);
    let ctor_root = ctor_value;
    let species_getter = native_static_with_value_roots(
        heap,
        "get [Symbol.species]",
        0,
        species_get,
        &[&global_root, &ctor_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let species_sym = well_known.get(WellKnown::Species);
    let installed = if let Some(f) = ctor_value.as_native_function() {
        f.define_own_symbol_property(
            heap,
            species_sym,
            crate::object::PartialPropertyDescriptor {
                get: Some(Value::native_function(species_getter)),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        )
    } else if let Some(obj) = ctor_value.as_object() {
        crate::object::define_own_symbol_property_partial(
            obj,
            heap,
            species_sym,
            crate::object::PartialPropertyDescriptor {
                get: Some(Value::native_function(species_getter)),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
        true
    } else {
        return Ok(());
    };
    if !installed {
        return Err(JsSurfaceError::DefinePropertyFailed(
            "constructor[Symbol.species]",
        ));
    }
    Ok(())
}
