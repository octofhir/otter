//! `%Symbol%` constructor installer + post-bootstrap well-known wiring.
//!
//! Implements ECMA-262 §20.4 Symbol Objects: the `Symbol()` ordinary
//! function (callable, not constructible), the global registry helpers
//! `Symbol.for` / `Symbol.keyFor`, every well-known symbol as a
//! data property on the constructor, and the post-bootstrap pass that
//! patches well-known symbol prototypes once `%Object.prototype%` is
//! reachable.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-symbol-constructor>

use crate::Value;
use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, define_global,
    install_iterator_well_knowns_post_bootstrap, native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject, PropertyDescriptor};

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

                let _string_heap = ctx.heap_mut();
                let rendered = crate::string::JsString::from_str(&coerced, ctx.heap_mut())
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
        Ok(Value::symbol(sym))
    }

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
                let value =
                    crate::string::JsString::from_str(&key, ctx.heap_mut()).map_err(|_| {
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

    fn symbol_proto_to_string(
        ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        let this = *ctx.this_value();
        let sym = if let Some(sym) = this.as_symbol(ctx.heap()) {
            sym
        } else if let Some(obj) = this.as_object() {
            let heap = ctx.interp_mut().gc_heap();
            crate::object::symbol_data(obj, heap).ok_or_else(|| NativeError::TypeError {
                name: "Symbol.prototype.toString",
                reason: "this is not a Symbol".to_string(),
            })?
        } else {
            return Err(NativeError::TypeError {
                name: "Symbol.prototype.toString",
                reason: "this is not a Symbol".to_string(),
            });
        };

        let s =
            crate::string::JsString::from_str(&sym.descriptive_string(ctx.heap()), ctx.heap_mut())
                .map_err(|_| NativeError::TypeError {
                    name: "Symbol.prototype.toString",
                    reason: "out of memory".to_string(),
                })?;
        Ok(Value::string(s))
    }

    fn symbol_proto_value_of(
        ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        let this = *ctx.this_value();
        if let Some(sym) = this.as_symbol(ctx.heap()) {
            return Ok(Value::symbol(sym));
        }
        if let Some(obj) = this.as_object() {
            let heap = ctx.interp_mut().gc_heap();
            return crate::object::symbol_data(obj, heap)
                .map(Value::symbol)
                .ok_or_else(|| NativeError::TypeError {
                    name: "Symbol.prototype.valueOf",
                    reason: "this is not a Symbol".to_string(),
                });
        }
        Err(NativeError::TypeError {
            name: "Symbol.prototype.valueOf",
            reason: "this is not a Symbol".to_string(),
        })
    }

    fn symbol_proto_to_primitive(
        ctx: &mut NativeCtx<'_>,
        _args: &[Value],
    ) -> Result<Value, NativeError> {
        let this = *ctx.this_value();
        if let Some(sym) = this.as_symbol(ctx.heap()) {
            return Ok(Value::symbol(sym));
        }
        if let Some(obj) = this.as_object() {
            let heap = ctx.interp_mut().gc_heap();
            return crate::object::symbol_data(obj, heap)
                .map(Value::symbol)
                .ok_or_else(|| NativeError::TypeError {
                    name: "Symbol.prototype[@@toPrimitive]",
                    reason: "this is not a Symbol".to_string(),
                });
        }
        Err(NativeError::TypeError {
            name: "Symbol.prototype[@@toPrimitive]",
            reason: "this is not a Symbol".to_string(),
        })
    }

    // The Symbol constructor itself is a callable NativeFunction.
    let global_root = Value::object(global);
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
    let symbol_ctor_root = Value::native_function(symbol_ctor);
    let prototype = alloc_object_with_value_roots(heap, &[&global_root, &symbol_ctor_root])?;
    if let Some(object_ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object())
        && let Some(object_proto) =
            object::get(object_ctor, heap, "prototype").and_then(|v| v.as_object())
    {
        object::set_prototype(prototype, heap, Some(object_proto));
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

    {
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            prototype,
            vec![global_root, symbol_ctor_root],
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
        let prototype_root = Value::object(prototype);
        let getter = native_static_with_value_roots(
            heap,
            "get description",
            0,
            symbol_proto_description_get,
            &[&global_root, &symbol_ctor_root, &prototype_root],
        )
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
        let desc_desc =
            PropertyDescriptor::accessor(Some(Value::native_function(getter)), None, false, true);
        if !object::define_own_property(prototype, heap, "description", desc_desc) {
            return Err(JsSurfaceError::DefinePropertyFailed("description"));
        }
    }
    // Install Symbol.prototype as an own property on the constructor.
    let proto_desc = PropertyDescriptor::data(Value::object(prototype), false, false, false);
    if !symbol_ctor.define_own_property(heap, "prototype", proto_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    }
    // Well-known symbol own properties (`Symbol.iterator`,
    // `Symbol.toPrimitive`, …) are installed by
    // [`install_symbol_well_knowns_post_bootstrap`] once the
    // per-interpreter `WellKnownSymbols` singleton table exists.
    // `for` / `keyFor` methods.
    let prototype_root = Value::object(prototype);
    let symbol_for_fn = native_static_with_value_roots(
        heap,
        "for",
        1,
        symbol_for_call,
        &[&global_root, &symbol_ctor_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let symbol_for_root = Value::native_function(symbol_for_fn);
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
        PropertyDescriptor::data(Value::native_function(symbol_for_fn), true, false, true);
    let key_for_desc =
        PropertyDescriptor::data(Value::native_function(symbol_key_for_fn), true, false, true);
    if !symbol_ctor.define_own_property(heap, "for", for_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("for"));
    }
    if !symbol_ctor.define_own_property(heap, "keyFor", key_for_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("keyFor"));
    }
    // Install Symbol.prototype.constructor → Symbol.
    let constructor_desc =
        PropertyDescriptor::data(Value::native_function(symbol_ctor), true, false, true);
    if !object::define_own_property(prototype, heap, "constructor", constructor_desc) {
        return Err(JsSurfaceError::DefinePropertyFailed("constructor"));
    }
    // Symbol.prototype[@@toPrimitive] is installed by
    // `install_symbol_well_knowns_post_bootstrap` so it points at
    // the per-realm well-known JsSymbol singleton.
    let _ = WellKnown::Iterator; // silence the unused-import lint
    let _ = symbol_proto_to_primitive;
    define_global(global, heap, "Symbol", Value::native_function(symbol_ctor));
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
    // §24.* — install collection `@@iterator` / `@@toStringTag`.
    crate::bootstrap_collections::install_collection_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    // §27.2.5.5 — install `Promise.prototype[@@toStringTag]`.
    crate::bootstrap_promise::install_promise_well_knowns_post_bootstrap(heap, global, well_known)?;
    // §26.1.4.4 / §26.2.4.5 — `WeakRef.prototype[@@toStringTag]`
    // + `FinalizationRegistry.prototype[@@toStringTag]`.
    crate::bootstrap_weak_refs::install_weak_well_knowns_post_bootstrap(heap, global, well_known)?;
    // §21.2.5 — `BigInt.prototype[@@toStringTag]`.
    crate::bootstrap_bigint::install_bigint_well_knowns_post_bootstrap(heap, global, well_known)?;
    // §25.3.5 — `DataView.prototype[@@toStringTag]`.
    crate::bootstrap_data_view::install_data_view_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    // §23.2.4 — `%TypedArray%.prototype[@@iterator]` plus per-kind
    // `<T>.prototype[@@toStringTag]`.
    crate::bootstrap_typed_array::install_typed_array_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    // §27.1.2 — `Iterator.prototype[@@iterator]` (returns this) and
    // `[@@toStringTag] = "Iterator"`.
    install_iterator_well_knowns_post_bootstrap(heap, global, well_known)?;
    // §25.1.5 — `ArrayBuffer.prototype[@@toStringTag]`.
    crate::bootstrap_array_buffer::install_array_buffer_well_knowns_post_bootstrap(
        heap, global, well_known,
    )?;
    // §21.4.4.45 — `Date.prototype[@@toPrimitive]`.
    crate::date::well_known::install_date_well_knowns_post_bootstrap(heap, global, well_known)?;
    // §22.1.3.34 — `String.prototype[@@iterator]`.
    crate::install_string_iterator_post_bootstrap(heap, global, well_known)?;
    // §22.2.6.{8,10} — `RegExp.prototype[@@match]` / `[@@search]`.
    crate::bootstrap_regexp::install_regexp_well_knowns_post_bootstrap(heap, global, well_known)?;
    // §25.2.5 — `SharedArrayBuffer.prototype[@@toStringTag]`.
    crate::bootstrap_array_buffer::install_shared_array_buffer_well_knowns_post_bootstrap(
        heap, global, well_known,
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

/// `BuiltinIntrinsic` adapter for the global `Symbol` constructor.
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Symbol";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_symbol(heap, global)
    }
}
