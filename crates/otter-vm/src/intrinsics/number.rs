//! `%Number%` constructor installer.
//!
//! Implements ECMA-262 §21.1 Number Objects: the `Number()` constructor,
//! every static property/method (`NaN`, `MAX_VALUE`, `isFinite`,
//! `isInteger`, …), and `Number.prototype` wiring.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-number-objects>

use crate::Value;
use crate::bootstrap::{
    BootstrapFeatures, alloc_object_with_value_roots, define_global, native_static_with_value_roots,
};
use crate::intrinsic_install::BuiltinIntrinsic;
use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::object::{self, JsObject};

fn install_number(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
    use crate::{NativeCall, NativeCtx, NativeError};

    let global_root = Value::object(global);
    // Number.prototype with all the formatter methods + the
    // hidden `[[NumberData]]` slot (= +0 per §21.1.3) so
    // `Number.prototype.toString()` recovers the value.
    let prototype = alloc_object_with_value_roots(heap, &[&global_root])?;
    {
        let mut builder =
            ObjectBuilder::from_object_with_value_roots(heap, prototype, vec![global_root]);
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
            let this = *ctx.this_value();
            if let Some(obj) = this.as_object() {
                crate::object::set_number_data(obj, ctx.heap_mut(), value);
                Ok(Value::object(obj))
            } else {
                Err(NativeError::TypeError {
                    name: "Number",
                    reason: "expected object receiver in `new Number(...)`".to_string(),
                })
            }
        } else {
            Ok(Value::number(value))
        }
    }

    let prototype_root = Value::object(prototype);
    let ctor_native = native_static_with_value_roots(
        heap,
        "Number",
        1,
        number_ctor_call,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let ctor_native_root = Value::native_function(ctor_native);
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
    if let Some(object_ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object())
        && let Some(object_proto) =
            object::get(object_ctor, heap, "prototype").and_then(|v| v.as_object())
    {
        object::set_prototype(statics, heap, Some(object_proto));
    }
    // Same chaining for `Number.prototype`, so
    // `Number.prototype.hasOwnProperty(...)` resolves.
    if let Some(object_ctor) = object::get(global, heap, "Object").and_then(|v| v.as_object())
        && let Some(object_proto) =
            object::get(object_ctor, heap, "prototype").and_then(|v| v.as_object())
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
        crate::object::PropertyDescriptor::data(Value::object(prototype), false, false, false),
    );
    // §21.1.2 — `Number.length` is a non-writable, non-enumerable,
    // configurable data property whose value matches the formal
    // parameter count of the constructor (1).
    let _ = object::define_own_property(
        statics,
        heap,
        "length",
        crate::object::PropertyDescriptor::data(Value::number_i32(1), false, false, true),
    );
    // §21.1.2 — `Number.name` is `"Number"`, non-writable,
    // non-enumerable, configurable.
    let number_name_value = Value::string(
        crate::string::JsString::from_str("Number", heap)
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
            vec![global_root, prototype_root],
        );
        for (name, value) in constants {
            builder.property(name, Value::number_f64(*value), Attr::read_only())?;
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
        let result = args
            .first()
            .and_then(|v| v.as_number())
            .is_some_and(|n| n.as_f64().is_nan());
        Ok(Value::boolean(result))
    }
    fn number_is_finite_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let result = args
            .first()
            .and_then(|v| v.as_number())
            .is_some_and(|n| n.as_f64().is_finite());
        Ok(Value::boolean(result))
    }
    fn number_is_integer_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(crate::number::parse::is_integer(&v)))
    }
    fn number_is_safe_integer_native(
        _ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let v = args.first().cloned().unwrap_or(Value::undefined());
        Ok(Value::boolean(crate::number::parse::is_safe_integer(&v)))
    }
    fn number_parse_int_native(
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = if let Some(arg) = args.first() {
            if let Some(s) = arg.as_string(ctx.heap()) {
                s.to_lossy_string(ctx.heap())
            } else {
                arg.display_string(ctx.heap())
            }
        } else {
            return Ok(Value::number(crate::number::NumberValue::from_f64(
                f64::NAN,
            )));
        };
        let radix = args
            .get(1)
            .and_then(|v| v.as_number())
            .map_or(0, |n| n.as_f64() as i32);
        Ok(Value::number(crate::number::parse::parse_int(&s, radix)))
    }
    fn number_parse_float_native(
        ctx: &mut NativeCtx<'_>,
        args: &[Value],
    ) -> Result<Value, NativeError> {
        let s = if let Some(arg) = args.first() {
            if let Some(s) = arg.as_string(ctx.heap()) {
                s.to_lossy_string(ctx.heap())
            } else {
                arg.display_string(ctx.heap())
            }
        } else {
            return Ok(Value::number(crate::number::NumberValue::from_f64(
                f64::NAN,
            )));
        };
        Ok(Value::number(crate::number::parse::parse_float(&s)))
    }

    {
        let global_root2 = Value::object(global);
        let statics_root = Value::object(statics);
        let prototype_root2 = Value::object(prototype);
        let mut builder = ObjectBuilder::from_object_with_value_roots(
            heap,
            statics,
            vec![global_root2, prototype_root2],
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
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::EncodeURI,
                args,
                ctx.heap_mut(),
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
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::EncodeURIComponent,
                args,
                ctx.heap_mut(),
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
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::DecodeURI,
                args,
                ctx.heap_mut(),
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
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::DecodeURIComponent,
                args,
                ctx.heap_mut(),
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
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::Escape,
                args,
                ctx.heap_mut(),
            )
            .map_err(|err| NativeError::TypeError {
                name: "escape",
                reason: err.to_string(),
            })
        }
        fn global_unescape(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            crate::global_functions::call(
                otter_bytecode::method_id::GlobalMethod::Unescape,
                args,
                ctx.heap_mut(),
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
            let arg = args.first().cloned().unwrap_or(Value::undefined());
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

    let number_value = Value::object(statics);
    // §21.1.3.1 `Number.prototype.constructor` points back at the
    // Number constructor.
    let _ = object::define_own_property(
        prototype,
        heap,
        "constructor",
        crate::object::PropertyDescriptor::data(number_value, true, false, true),
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

/// `BuiltinIntrinsic` adapter for the global `Number` constructor.
pub struct Intrinsic;

impl BuiltinIntrinsic for Intrinsic {
    const NAME: &'static str = "Number";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_number(heap, global)
    }
}
