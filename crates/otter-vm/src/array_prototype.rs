//! `Array.prototype.*` interpreter-aware drivers.
//!
//! Every `Array.prototype` method is a [`NativeCtx`] native that
//! re-enters the matching `Interpreter::array_*` driver, so direct
//! `arr.m()` calls and `Array.prototype.m.call(...)` share one
//! implementation. The drivers run the spec algorithms with live VM
//! property operations (`Get`, `Set`, `LengthOfArrayLike`, species
//! constructors, callbacks, comparators, coercions).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-prototype-object>
//! - `docs/method-dispatch-refactor.md`
//!
//! # Contents
//! - [`ARRAY_PROTOTYPE_METHODS`] — JS-visible native method specs.
//! - `native_array_method` — null/undefined guard then driver dispatch.
//! - `Interpreter::array_*` drivers for live generic Array semantics.
//!
//! # Invariants
//! - Generic methods begin with `ToObject(this)` and
//!   `LengthOfArrayLike` in the driver path.
//! - Live drivers use VM property operations so accessors, inherited
//!   indices, proxies, callbacks, and comparator calls re-enter through
//!   the active [`ExecutionContext`].
//! - Pathological array-like lengths are guarded before any dense
//!   materialisation.

use smallvec::SmallVec;

use crate::Value;
use crate::js_surface::{Attr, JsSurfaceError, MethodSpec};
use crate::number::NumberValue;
use crate::object::{self, PartialPropertyDescriptor};
use crate::string::JsString;
use crate::symbol::{WellKnown, WellKnownSymbols};
use crate::{ExecutionContext, Interpreter, NativeCall, NativeCtx, NativeError, VmError};

/// Defensive upper bound on the materialised length of an
/// array-like Object receiver before we'd refuse to expand a
/// snapshot. Spec ToLength clamps to 2^53-1; test262 patterns
/// (`{length: 2**32 - 1}`, `new Array(2**32)`) deliberately
/// exercise pathological lengths to stress generic-array methods.
/// V8 / JSC handle this by visiting only **present** indexed own
/// properties (see HasProperty short-circuit in §22.1.3 generic
/// algorithms); we follow the same strategy and never materialise
/// the absent slots, so the cap only matters when a caller passes
/// in a genuinely-large-but-dense object — at that point an OOM
/// `RangeError` from the allocator is the spec-compliant outcome
/// and we never reach a 4 GB pre-allocated `Vec`.
const MAX_ARRAY_LIKE_PROBE_LEN: usize = 1 << 25;
const MAX_SPARSE_PREFIX_PROBE_LEN: usize = 1024;
const MAX_SAFE_ARRAY_LENGTH: usize = 9_007_199_254_740_991;

/// Static `Array.prototype` method specs.
pub static ARRAY_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("push", 1, native_push),
    method("pop", 0, native_pop),
    method("shift", 0, native_shift),
    method("unshift", 1, native_unshift),
    method("slice", 2, native_slice),
    method("concat", 1, native_concat),
    method("join", 1, native_join),
    method("includes", 1, native_includes),
    method("indexOf", 1, native_index_of),
    method("lastIndexOf", 1, native_last_index_of),
    method("at", 1, native_at),
    method("reverse", 0, native_reverse),
    method("fill", 3, native_fill),
    method("flat", 0, native_flat),
    method("splice", 2, native_splice),
    method("sort", 1, native_sort),
    method("toString", 0, native_to_string),
    method("copyWithin", 2, native_copy_within),
    method("toReversed", 0, native_to_reversed),
    method("toSpliced", 2, native_to_spliced),
    method("toSorted", 1, native_to_sorted),
    method("with", 2, native_with),
    method("toLocaleString", 0, native_to_locale_string),
    method("keys", 0, native_keys_iter),
    method("values", 0, native_values_iter),
    method("entries", 0, native_entries_iter),
    method("forEach", 1, native_for_each),
    method("map", 1, native_map),
    method("filter", 1, native_filter),
    method("some", 1, native_some),
    method("every", 1, native_every),
    method("find", 1, native_find),
    method("findIndex", 1, native_find_index),
    method("findLast", 1, native_find_last),
    method("findLastIndex", 1, native_find_last_index),
    method("reduce", 1, native_reduce),
    method("reduceRight", 1, native_reduce_right),
    method("flatMap", 1, native_flat_map),
];

/// Installs `Array.prototype.values` and `Array.prototype[Symbol.iterator]`.
pub(crate) fn install_array_well_knowns_post_bootstrap(
    heap: &mut otter_gc::GcHeap,
    global: object::JsObject,
    well_known: &WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    let Some(array_ctor) = object::get(global, heap, "Array").and_then(|v| v.as_native_function())
    else {
        return Ok(());
    };
    let Some(descriptor) = array_ctor
        .own_property_descriptor(&mut *heap, "prototype")
        .ok()
        .flatten()
    else {
        return Ok(());
    };
    let object::DescriptorKind::Data { value } = descriptor.kind else {
        return Ok(());
    };
    let Some(prototype) = value.as_object() else {
        return Ok(());
    };
    let global_root = Value::object(global);
    let prototype_root = Value::object(prototype);
    let values_fn = crate::bootstrap::native_static_with_value_roots(
        heap,
        "values",
        0,
        native_values_iter,
        &[&global_root, &prototype_root],
    )
    .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let values_value = Value::native_function(values_fn);
    object::define_own_property_partial(
        prototype,
        heap,
        "values",
        PartialPropertyDescriptor {
            value: Some(values_value),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        well_known.get(WellKnown::Iterator),
        PartialPropertyDescriptor {
            value: Some(values_value),
            writable: Some(true),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    install_array_unscopables(heap, prototype, well_known)?;
    Ok(())
}

/// §23.1.3.34 `Array.prototype[@@unscopables]` — a `null`-prototype
/// object whose own enumerable data properties (all `true`) name the
/// post-ES5 methods that a `with` statement must not bind. The list is
/// fixed by the spec; `with` itself is deliberately excluded. The
/// `@@unscopables` property is non-writable / non-enumerable /
/// configurable.
fn install_array_unscopables(
    heap: &mut otter_gc::GcHeap,
    prototype: object::JsObject,
    well_known: &WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    const UNSCOPABLES: &[&str] = &[
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ];
    let prototype_root = Value::object(prototype);
    let list =
        crate::intrinsics::shared::alloc_object_with_value_roots_pub(heap, &[&prototype_root])
            .map_err(|_| JsSurfaceError::OutOfMemory)?;
    object::set_prototype(list, heap, None);
    for name in UNSCOPABLES {
        object::define_own_property_partial(
            list,
            heap,
            name,
            PartialPropertyDescriptor {
                value: Some(Value::boolean(true)),
                writable: Some(true),
                enumerable: Some(true),
                configurable: Some(true),
                ..Default::default()
            },
        );
    }
    object::define_own_symbol_property_partial(
        prototype,
        heap,
        well_known.get(WellKnown::Unscopables),
        PartialPropertyDescriptor {
            value: Some(Value::object(list)),
            writable: Some(false),
            enumerable: Some(false),
            configurable: Some(true),
            ..Default::default()
        },
    );
    Ok(())
}

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

fn native_array_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    // §22.1.3 step 1 — every generic `Array.prototype.*` opens with
    // `ToObject(this value)`, which throws a TypeError on `null` /
    // `undefined` (RequireObjectCoercible).
    if receiver.is_null() || receiver.is_undefined() {
        return Err(NativeError::TypeError {
            name,
            reason: "Array.prototype method called on null or undefined".to_string(),
        });
    }
    let Some(exec) = ctx.execution_context().cloned() else {
        return Err(NativeError::TypeError {
            name,
            reason: "Array.prototype method requires an execution context".to_string(),
        });
    };
    let interp = ctx.interp_mut();
    match interp.array_live_method_dispatch(&exec, name, receiver, args, &[args]) {
        Some(result) => result.map_err(|err| crate::native_function::vm_to_native_error(err, name)),
        None => Err(NativeError::TypeError {
            name,
            reason: "unknown Array.prototype method".to_string(),
        }),
    }
}
macro_rules! native_array {
    ($fn_name:ident, $js_name:literal) => {
        fn $fn_name(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
            native_array_method($js_name, ctx, args)
        }
    };
}

native_array!(native_push, "push");
native_array!(native_pop, "pop");
native_array!(native_shift, "shift");
native_array!(native_unshift, "unshift");
native_array!(native_slice, "slice");
native_array!(native_concat, "concat");
native_array!(native_join, "join");
native_array!(native_includes, "includes");
native_array!(native_index_of, "indexOf");
native_array!(native_last_index_of, "lastIndexOf");
native_array!(native_at, "at");
native_array!(native_reverse, "reverse");
native_array!(native_fill, "fill");
native_array!(native_flat, "flat");
native_array!(native_splice, "splice");
native_array!(native_sort, "sort");
native_array!(native_to_string, "toString");
native_array!(native_copy_within, "copyWithin");
native_array!(native_to_reversed, "toReversed");
native_array!(native_to_spliced, "toSpliced");
native_array!(native_to_sorted, "toSorted");
native_array!(native_with, "with");
native_array!(native_to_locale_string, "toLocaleString");
native_array!(native_keys_iter, "keys");
native_array!(native_values_iter, "values");
native_array!(native_entries_iter, "entries");

/// §7.3.18 `LengthOfArrayLike(O)` with live `Get(O, "length")` semantics.
pub(crate) fn length_of_array_like(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    o: &Value,
) -> Result<usize, VmError> {
    if let Some(obj) = o.as_object()
        && let Some(s) = crate::object::string_data(obj, interp.gc_heap())
    {
        return Ok(s.len() as usize);
    }
    if let Some(arr) = o.as_array() {
        return Ok(crate::array::len(arr, interp.gc_heap()));
    }
    let len_val = interp.get_property_value_for_call(context, *o, "length")?;
    // §7.1.20 ToLength(? ToNumber(len)). A wrapper-object length
    // (`obj.length = new Number(4.5)`) or one with a `valueOf` must run
    // the numeric coercion ladder, not just match an existing Number.
    let len_val = if len_val.is_object_type() {
        interp.evaluate_to_primitive(
            context,
            &len_val,
            crate::abstract_ops::ToPrimitiveHint::Number,
        )?
    } else {
        len_val
    };
    crate::to_length(&len_val, interp.gc_heap())
}

/// §23.1.3.17 / §23.1.3.22 live search for `indexOf` and `lastIndexOf`.
pub(crate) fn array_linear_search(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    o: Value,
    name: &str,
    search: Value,
    from_arg: Option<Value>,
) -> Result<i64, VmError> {
    // §23.1.3.* step 2 — len = ? LengthOfArrayLike(O).
    let len = length_of_array_like(interp, context, &o)?;
    if len == 0 {
        return Ok(-1);
    }
    // §7.1.5 ToIntegerOrInfinity(fromIndex) begins with ToNumber →
    // ToPrimitive(Number); run user `valueOf` / `@@toPrimitive` on an
    // object `fromIndex` before the numeric clamp below.
    let from_arg = match from_arg {
        Some(v) if v.is_object_type() => Some(interp.evaluate_to_primitive(
            context,
            &v,
            crate::abstract_ops::ToPrimitiveHint::Number,
        )?),
        other => other,
    };
    let to_int = |v: &Value, heap: &otter_gc::GcHeap| -> f64 {
        let n = crate::number::parse::to_number_value(v, heap);
        if n.is_nan() { 0.0 } else { n.trunc() }
    };
    let len_i = len as i64;
    // String primitives / `String` wrappers expose code-unit indices
    // through `[[StringData]]`, which the ordinary `[[Get]]` /
    // `[[HasProperty]]` ladder may not surface. Resolve those indices
    // directly; `len` is already the string length, so inherited
    // beyond-length indices (`String.prototype[3]`) are never probed.
    let string_data = if let Some(obj) = o.as_object() {
        crate::object::string_data(obj, interp.gc_heap())
    } else {
        o.as_string(interp.gc_heap())
    };
    let probe = |interp: &mut Interpreter, k: i64| -> Result<Option<i64>, VmError> {
        if let Some(s) = string_data {
            let Some(unit) = s.char_code_at(k as u32, interp.gc_heap()) else {
                return Ok(None);
            };
            let ch = crate::string::JsString::from_utf16_units(&[unit], interp.gc_heap_mut())
                .map(Value::string)?;
            return Ok(
                if crate::abstract_ops::is_strictly_equal(&ch, &search, interp.gc_heap()) {
                    Some(k)
                } else {
                    None
                },
            );
        }
        let key = k.to_string();
        let has = interp.ordinary_has_property_value(
            context,
            o,
            &crate::VmPropertyKey::String(&key),
            0,
        )?;
        if !has {
            return Ok(None);
        }
        let v = interp.get_property_value_for_call(context, o, &key)?;
        if crate::abstract_ops::is_strictly_equal(&v, &search, interp.gc_heap()) {
            Ok(Some(k))
        } else {
            Ok(None)
        }
    };
    if name == "indexOf" {
        let n = from_arg.map_or(0.0, |v| to_int(&v, interp.gc_heap()));
        let mut k = if n >= len as f64 {
            len_i
        } else if n >= 0.0 {
            n as i64
        } else {
            (len_i + n as i64).max(0)
        };
        while k < len_i {
            if let Some(idx) = probe(interp, k)? {
                return Ok(idx);
            }
            k += 1;
        }
        Ok(-1)
    } else {
        // lastIndexOf — default fromIndex is len-1.
        let n = from_arg.map_or((len - 1) as f64, |v| to_int(&v, interp.gc_heap()));
        let mut k = if n >= 0.0 {
            (n as i64).min(len_i - 1)
        } else {
            len_i + n as i64
        };
        while k >= 0 {
            if let Some(idx) = probe(interp, k)? {
                return Ok(idx);
            }
            k -= 1;
        }
        Ok(-1)
    }
}

/// §23.1.3.16 live `Array.prototype.includes` with `SameValueZero`.
pub(crate) fn array_includes(
    interp: &mut Interpreter,
    context: &ExecutionContext,
    o: Value,
    search: Value,
    from_arg: Option<Value>,
) -> Result<bool, VmError> {
    let len = length_of_array_like(interp, context, &o)?;
    if len == 0 {
        return Ok(false);
    }
    // §7.1.5 ToIntegerOrInfinity(fromIndex): ToNumber → ToPrimitive(Number).
    let from_arg = match from_arg {
        Some(v) if v.is_object_type() => Some(interp.evaluate_to_primitive(
            context,
            &v,
            crate::abstract_ops::ToPrimitiveHint::Number,
        )?),
        other => other,
    };
    let len_i = len as i64;
    let n = match from_arg {
        Some(v) => {
            let f = crate::number::parse::to_number_value(&v, interp.gc_heap());
            if f.is_nan() { 0.0 } else { f.trunc() }
        }
        None => 0.0,
    };
    let mut k = if n >= len as f64 {
        return Ok(false);
    } else if n >= 0.0 {
        n as i64
    } else {
        (len_i + n as i64).max(0)
    };
    let string_data = if let Some(obj) = o.as_object() {
        crate::object::string_data(obj, interp.gc_heap())
    } else {
        o.as_string(interp.gc_heap())
    };
    while k < len_i {
        let v = if let Some(s) = string_data {
            match s.char_code_at(k as u32, interp.gc_heap()) {
                Some(unit) => {
                    crate::string::JsString::from_utf16_units(&[unit], interp.gc_heap_mut())
                        .map(Value::string)?
                }
                None => Value::undefined(),
            }
        } else {
            let key = k.to_string();
            interp.get_property_value_for_call(context, o, &key)?
        };
        if crate::abstract_ops::same_value_zero(&v, &search, interp.gc_heap()) {
            return Ok(true);
        }
        k += 1;
    }
    Ok(false)
}

impl Interpreter {
    /// §23.1.3 indexed search entry for `includes`, `indexOf`, and `lastIndexOf`.
    pub(crate) fn array_indexed_search(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        name: &str,
        search: Value,
        from_arg: Option<Value>,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        if name == "includes" {
            let found = array_includes(self, context, o, search, from_arg)?;
            Ok(Value::boolean(found))
        } else {
            let idx = array_linear_search(self, context, o, name, search, from_arg)?;
            Ok(Value::number(NumberValue::from_f64(idx as f64)))
        }
    }

    /// Routes NativeCtx Array prototype calls to their live interpreter drivers.
    pub(crate) fn array_live_method_dispatch(
        &mut self,
        context: &ExecutionContext,
        name: &str,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Option<Result<Value, VmError>> {
        match name {
            "indexOf" | "lastIndexOf" | "includes" => {
                let search = args.first().copied().unwrap_or_else(Value::undefined);
                let from_arg = args.get(1).copied();
                Some(self.array_indexed_search(context, receiver, name, search, from_arg, roots))
            }
            "join" => {
                let separator_arg = args.first().copied();
                Some(self.array_join(context, receiver, separator_arg, roots))
            }
            "toString" => Some(self.array_join(context, receiver, None, roots)),
            "toLocaleString" => Some(self.array_to_locale_string(context, receiver, roots)),
            "concat" => Some(self.array_concat(context, receiver, args, roots)),
            "sort" => {
                let comparefn = args.first().copied().unwrap_or_else(Value::undefined);
                Some(self.array_sort(context, receiver, comparefn, roots))
            }
            "push" => Some(self.array_push(context, receiver, args, roots)),
            "pop" => Some(self.array_pop(context, receiver, roots)),
            "shift" => Some(self.array_shift(context, receiver, roots)),
            "unshift" => Some(self.array_unshift(context, receiver, args, roots)),
            "at" => {
                let index = args.first().copied().unwrap_or_else(Value::undefined);
                Some(self.array_at(context, receiver, index, roots))
            }
            "reverse" => Some(self.array_reverse(context, receiver, roots)),
            "fill" => {
                let value = args.first().copied().unwrap_or_else(Value::undefined);
                Some(self.array_fill(context, receiver, value, args, roots))
            }
            "flat" => {
                let depth = args.first().copied().unwrap_or_else(Value::undefined);
                Some(self.array_flat(context, receiver, depth, roots))
            }
            "copyWithin" => Some(self.array_copy_within(context, receiver, args, roots)),
            "slice" => Some(self.array_slice(context, receiver, args, roots)),
            "splice" => Some(self.array_splice(context, receiver, args, roots)),
            "toReversed" => Some(self.array_to_reversed(context, receiver, roots)),
            "toSpliced" => Some(self.array_to_spliced(context, receiver, args, roots)),
            "toSorted" => {
                let comparefn = args.first().copied().unwrap_or_else(Value::undefined);
                Some(self.array_to_sorted(context, receiver, comparefn, roots))
            }
            "with" => {
                let index = args.first().copied().unwrap_or_else(Value::undefined);
                let value = args.get(1).copied().unwrap_or_else(Value::undefined);
                Some(self.array_with(context, receiver, index, value, roots))
            }
            "keys" => Some(self.array_iterator_method(context, receiver, "keys", roots)),
            "values" => Some(self.array_iterator_method(context, receiver, "values", roots)),
            "entries" => Some(self.array_iterator_method(context, receiver, "entries", roots)),
            _ => None,
        }
    }

    /// §23.1.3.1 live `Array.prototype.at`.
    pub(crate) fn array_at(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        index: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let n = self.coerce_to_number(context, &index)?.as_f64();
        let relative = if n.is_nan() {
            0.0
        } else if n.is_infinite() {
            n
        } else {
            n.trunc()
        };
        let actual = if relative < 0.0 {
            len as f64 + relative
        } else {
            relative
        };
        if !actual.is_finite() || actual < 0.0 || actual >= len as f64 {
            return Ok(Value::undefined());
        }
        self.array_method_get_property(context, o, &format_index_key(actual))
    }

    /// §23.1.3.27 live `Array.prototype.reverse`.
    pub(crate) fn array_reverse(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        if len < 2 {
            return Ok(o);
        }
        let bounded_len = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        for lower in 0..(bounded_len / 2) {
            let upper = len - lower - 1;
            let lower_key = lower.to_string();
            let upper_key = format_index_key(upper as f64);
            let lower_exists = self.array_method_has_property(context, o, &lower_key)?;
            let upper_exists = self.array_method_has_property(context, o, &upper_key)?;
            let lower_value = if lower_exists {
                Some(self.array_method_get_property(context, o, &lower_key)?)
            } else {
                None
            };
            let upper_value = if upper_exists {
                Some(self.array_method_get_property(context, o, &upper_key)?)
            } else {
                None
            };
            match (lower_value, upper_value) {
                (Some(l), Some(u)) => {
                    self.array_set_property_throwing(context, o, &lower_key, u)?;
                    self.array_set_property_throwing(context, o, &upper_key, l)?;
                }
                (Some(l), None) => {
                    self.array_delete_property_throwing(context, o, &lower_key)?;
                    self.array_set_property_throwing(context, o, &upper_key, l)?;
                }
                (None, Some(u)) => {
                    self.array_set_property_throwing(context, o, &lower_key, u)?;
                    self.array_delete_property_throwing(context, o, &upper_key)?;
                }
                (None, None) => {}
            }
        }
        Ok(o)
    }

    /// §23.1.3.7 live `Array.prototype.fill`.
    pub(crate) fn array_fill(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        value: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let start = self.array_relative_index(context, args.get(1), 0.0, len)?;
        let end = self.array_relative_index(context, args.get(2), len as f64, len)?;
        let bounded_end = end.min(start.saturating_add(MAX_ARRAY_LIKE_PROBE_LEN));
        for k in start..bounded_end {
            self.array_set_property_throwing(context, o, &format_index_key(k as f64), value)?;
        }
        Ok(o)
    }

    /// §23.1.3.11 live `Array.prototype.flat`.
    /// §23.1.3.13 `Array.prototype.flat([depth])`.
    ///
    /// `A = ArraySpeciesCreate(O, 0)` then `FlattenIntoArray(A, O,
    /// sourceLen, 0, depthNum)`. The result honours the receiver's
    /// `@@species` constructor and installs each element via the
    /// observable `CreateDataPropertyOrThrow`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.prototype.flat>
    pub(crate) fn array_flat(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        depth_arg: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let source_len = length_of_array_like(self, context, &o)?.min(MAX_ARRAY_LIKE_PROBE_LEN);
        let depth = if depth_arg.is_undefined() {
            1
        } else {
            let n = self.coerce_to_number(context, &depth_arg)?.as_f64();
            if n.is_nan() || n <= 0.0 {
                0
            } else if n.is_infinite() {
                i64::MAX
            } else {
                n.trunc() as i64
            }
        };
        let a = self.array_species_create(context, o, 0, roots)?;
        let anchor_base = self.push_iteration_anchor(a) - 1;
        self.push_iteration_anchor(o);
        let result = self.flatten_into_array(context, a, o, source_len, 0, depth, None);
        self.pop_iteration_anchors_to(anchor_base);
        result?;
        Ok(a)
    }

    /// §7.2.2 `IsArray`: an Array exotic object, or a non-revoked
    /// Proxy whose target chain bottoms out in one. A revoked Proxy is
    /// an abrupt TypeError.
    pub(crate) fn is_array_spec(&self, value: &Value) -> Result<bool, VmError> {
        let mut current = *value;
        for _ in 0..=object::PROTO_CHAIN_HARD_CAP {
            if current.is_array() {
                return Ok(true);
            }
            let Some(proxy) = current.as_proxy() else {
                return Ok(false);
            };
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform 'IsArray' on a proxy that has been revoked"
                        .to_string(),
                });
            }
            current = proxy.target(&self.gc_heap);
        }
        Ok(false)
    }

    /// §23.1.3.13.1 FlattenIntoArray. Recurses into nested arrays while
    /// `depth > 0`, applying `mapper` (the `flatMap` callback) only at
    /// the top level. Reads each source index through the observable
    /// `HasProperty` / `Get`, recurses on `IsArray` elements, and
    /// installs leaves via `CreateDataPropertyOrThrow`. Returns the next
    /// free target index.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-flattenintoarray>
    pub(crate) fn flatten_into_array(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        source: Value,
        source_len: usize,
        start: usize,
        depth: i64,
        mapper: Option<(Value, Value)>,
    ) -> Result<usize, VmError> {
        // 2^53 - 1: a CreateDataPropertyOrThrow target index past the
        // safe-integer limit is a §23.1.3.13.1 step 4.c.ii TypeError.
        const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
        let mut target_index = start;
        for source_index in 0..source_len {
            let key = format_index_key(source_index as f64);
            if !self.array_method_has_property(context, source, &key)? {
                continue;
            }
            let mut element = self.array_method_get_property(context, source, &key)?;
            let anchor_base = self.push_iteration_anchor(element) - 1;
            if let Some((mapper_fn, map_this)) = mapper {
                let cb_args: SmallVec<[Value; 8]> =
                    smallvec::smallvec![element, Value::number_f64(source_index as f64), source,];
                element = self.run_callable_sync(context, &mapper_fn, map_this, cb_args)?;
                self.push_iteration_anchor(element);
            }
            if depth > 0 && self.is_array_spec(&element)? {
                let element_len =
                    length_of_array_like(self, context, &element)?.min(MAX_ARRAY_LIKE_PROBE_LEN);
                target_index = self.flatten_into_array(
                    context,
                    target,
                    element,
                    element_len,
                    target_index,
                    depth - 1,
                    None,
                )?;
            } else {
                if target_index as f64 >= MAX_SAFE_INTEGER {
                    self.pop_iteration_anchors_to(anchor_base);
                    return Err(VmError::TypeError {
                        message: "flatten target index exceeds maximum safe integer".to_string(),
                    });
                }
                self.create_data_property_or_throw(
                    context,
                    target,
                    &format_index_key(target_index as f64),
                    element,
                )?;
                target_index += 1;
            }
            self.pop_iteration_anchors_to(anchor_base);
        }
        Ok(target_index)
    }

    /// §23.1.3.6 / §23.1.3.20 / §23.1.3.32 live Array iterator creation.
    pub(crate) fn array_iterator_method(
        &mut self,
        _context: &ExecutionContext,
        receiver: Value,
        kind: &str,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        // §23.1.5.1 CreateArrayIterator(O, kind). A real array uses the
        // dense `Array*` states (its own GC handle reflects mutation); a
        // generic array-like (e.g. an `arguments` object) holds a live
        // `ArrayLike` state so a `length` / element change between
        // `next()` calls is observed rather than snapshot at creation.
        let state = if let Some(arr) = o.as_array() {
            match kind {
                "keys" => crate::IteratorState::ArrayKey {
                    array: arr,
                    index: 0,
                },
                "entries" => crate::IteratorState::ArrayEntry {
                    array: arr,
                    index: 0,
                },
                _ => crate::IteratorState::Array {
                    array: arr,
                    index: 0,
                    origin: crate::BuiltinIteratorOrigin::Array,
                },
            }
        } else {
            let iter_kind = match kind {
                "keys" => crate::iterator_state::ArrayIterKind::Key,
                "entries" => crate::iterator_state::ArrayIterKind::Entry,
                _ => crate::iterator_state::ArrayIterKind::Value,
            };
            crate::IteratorState::ArrayLike {
                object: o,
                index: 0,
                kind: iter_kind,
            }
        };
        let arr_root = o;
        let heap = self.gc_heap_mut();
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            arr_root.trace_value_slots(visit);
            for root in roots {
                for value in *root {
                    value.trace_value_slots(visit);
                }
            }
        };
        heap.alloc_with_roots(state, &mut visitor)
            .map(Value::iterator)
            .map_err(|_| VmError::TypeError {
                message: "iterator allocation failed".to_string(),
            })
    }

    /// §23.1.3.18 live `Array.prototype.join` over a generic array-like receiver.
    pub(crate) fn array_join(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        separator_arg: Option<Value>,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        // §23.1.3.16 step 1 — O = ToObject(this value).
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        // §23.1.3.16 step 2 — len = ? LengthOfArrayLike(O). Reads
        // `O.length` through `[[Get]]`, so a `get length()` accessor
        // fires here exactly once.
        let len = length_of_array_like(self, context, &o)?;
        // §23.1.3.16 step 3 — sep = (separator is undefined) ? ","
        // : ? ToString(separator). Ordered AFTER the length read.
        let separator = match separator_arg {
            None => ",".to_string(),
            Some(v) if v.is_undefined() => ",".to_string(),
            Some(v) => self.coerce_to_string(context, &v)?,
        };
        // Allocation is bounded by `MAX_ARRAY_LIKE_PROBE_LEN`, matching
        // `impl_join`, so a pathological `length` (`2**32`) never sizes a
        // multi-gigabyte parts buffer.
        let cap = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        if cap == 0 {
            return Ok(Value::string(JsString::from_str("", self.gc_heap_mut())?));
        }
        // Sparse-safe index gathering: present own indices `< len` from
        // the receiver and every prototype-chain object. An absent index
        // joins as the empty string, indistinguishable from a `Get`
        // returning `undefined`, so skipping it is spec-faithful for the
        // array-like generic case (same caveat `impl_join` carries).
        let mut indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, cap, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        // §23.1.3.16 steps 4-8 — element = ? Get(O, ToString(k)); the
        // element joins as "" when undefined / null, else ? ToString.
        let mut parts: Vec<String> = vec![String::new(); cap];
        for k in indices {
            if k >= cap {
                continue;
            }
            let v = self.get_property_value_for_call(context, o, &k.to_string())?;
            parts[k] = if v.is_undefined() || v.is_null() {
                String::new()
            } else {
                self.coerce_to_string(context, &v)?
            };
        }
        let joined = parts.join(&separator);
        Ok(Value::string(JsString::from_str(
            &joined,
            self.gc_heap_mut(),
        )?))
    }

    /// §23.1.3.32 `Array.prototype.toLocaleString`. Unlike `toString`
    /// (which joins via `ToString`), each present element is stringified
    /// through `ToString(? Invoke(element, "toLocaleString"))`, so the
    /// element's own `toLocaleString` runs with the element as `this`
    /// (primitive elements stay primitive under a strict callee). A
    /// `null` / `undefined` element contributes nothing but the
    /// separator is still emitted between every position.
    pub(crate) fn array_to_locale_string(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        // step 1 — array = ? ToObject(this value).
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        // step 2 — len = ? LengthOfArrayLike(array).
        let len = length_of_array_like(self, context, &o)?;
        // The list separator is implementation-defined; ECMA-402 absent,
        // use "," so `["",""].toLocaleString()` reports it consistently.
        const SEPARATOR: &str = ",";
        let cap = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        let mut r = String::new();
        for k in 0..cap {
            if k > 0 {
                r.push_str(SEPARATOR);
            }
            // step 12.b — element = ? Get(array, ToString(k)).
            let element = self.get_property_value_for_call(context, o, &k.to_string())?;
            if element.is_undefined() || element.is_null() {
                continue;
            }
            // step 12.e — Invoke(element, "toLocaleString"): GetV resolves
            // the method (boxing a primitive only for the lookup), then
            // Call passes the original element as `this`.
            let method = match self.ordinary_get_value(
                context,
                element,
                element,
                &crate::VmPropertyKey::String("toLocaleString"),
                0,
            )? {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, element, SmallVec::new())?
                }
            };
            if !crate::abstract_ops::is_callable(&method) {
                return Err(VmError::TypeError {
                    message: "element's toLocaleString is not callable".to_string(),
                });
            }
            let result = self.run_callable_sync(context, &method, element, SmallVec::new())?;
            let s = self.coerce_to_string(context, &result)?;
            r.push_str(&s);
        }
        Ok(Value::string(JsString::from_str(&r, self.gc_heap_mut())?))
    }

    /// §22.1.3.10.1 IsConcatSpreadable(O): a non-object is never spread;
    /// otherwise `Get(O, @@isConcatSpreadable)` decides when not
    /// undefined (ToBoolean), else `IsArray(O)`.
    fn is_concat_spreadable(
        &mut self,
        context: &ExecutionContext,
        e: Value,
    ) -> Result<bool, VmError> {
        if !e.is_object_type() {
            return Ok(false);
        }
        let sym = self.well_known_symbols.get(WellKnown::IsConcatSpreadable);
        let spread =
            match self.ordinary_get_value(context, e, e, &crate::VmPropertyKey::Symbol(sym), 0)? {
                crate::VmGetOutcome::Value(v) => v,
                crate::VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, e, smallvec::SmallVec::new())?
                }
            };
        if spread.is_undefined() {
            // §22.1.3.1.1 step 3 — fall back to IsArray(O), which per
            // §7.2.2 unwraps a Proxy to its target (throwing for a
            // revoked proxy) rather than only matching a bare Array.
            self.is_array_spec(&e)
        } else {
            Ok(spread.to_boolean(self.gc_heap()))
        }
    }

    /// §23.1.3.1 concat loop body, appending directly onto the
    /// species-created result `a` starting at index `n`: a spreadable
    /// `e` contributes `CreateDataProperty(a, n+k, Get(E, k))` for each
    /// present index (absent indices advance `n` without a property),
    /// else `e` is appended as one element. The combined length must
    /// stay within `2**53 - 1` (TypeError otherwise). Returns the next
    /// write index.
    fn concat_append_to(
        &mut self,
        context: &ExecutionContext,
        e: Value,
        a: Value,
        mut n: u64,
    ) -> Result<u64, VmError> {
        const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
        let too_long = || VmError::TypeError {
            message: "concatenated array length exceeds the maximum safe integer".to_string(),
        };
        if self.is_concat_spreadable(context, e)? {
            let len = length_of_array_like(self, context, &e)? as u64;
            if (n as f64) + (len as f64) > MAX_SAFE {
                return Err(too_long());
            }
            for k in 0..len {
                let key = k.to_string();
                if self.ordinary_has_property_value(
                    context,
                    e,
                    &crate::VmPropertyKey::String(&key),
                    0,
                )? {
                    let v = self.get_property_value_for_call(context, e, &key)?;
                    self.create_data_property_or_throw(context, a, &n.to_string(), v)?;
                }
                n += 1;
            }
        } else {
            if n as f64 >= MAX_SAFE {
                return Err(too_long());
            }
            self.create_data_property_or_throw(context, a, &n.to_string(), e)?;
            n += 1;
        }
        Ok(n)
    }

    /// §23.1.3.2 live `Array.prototype.concat` over a generic receiver.
    pub(crate) fn array_concat(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        // §23.1.3.1 step 2 — A = ArraySpeciesCreate(O, 0) runs (and its
        // observable `@@species` lookup / construction) before any
        // element is read; elements are then appended via
        // CreateDataProperty so a custom species object / proxy
        // observes each define.
        let a = self.array_species_create(context, o, 0, roots)?;
        let mut n: u64 = self.concat_append_to(context, o, a, 0)?;
        for &item in args {
            n = self.concat_append_to(context, item, a, n)?;
        }
        // §23.1.3.1 step 4 — Set(A, "length", n).
        self.array_set_property_throwing(
            context,
            a,
            "length",
            Value::number(NumberValue::from_f64(n as f64)),
        )?;
        Ok(a)
    }

    /// §23.1.3.30.1 SortCompare(x, y, comparefn). `undefined` sorts to
    /// the end; with a comparefn the result is `ToNumber(comparefn(x,y))`
    /// (NaN → equal); otherwise the ToString lexicographic order.
    fn sort_compare(
        &mut self,
        context: &ExecutionContext,
        x: Value,
        y: Value,
        comparefn: Value,
    ) -> Result<std::cmp::Ordering, VmError> {
        use std::cmp::Ordering;
        let x_undef = x.is_undefined();
        let y_undef = y.is_undefined();
        if x_undef && y_undef {
            return Ok(Ordering::Equal);
        }
        if x_undef {
            return Ok(Ordering::Greater);
        }
        if y_undef {
            return Ok(Ordering::Less);
        }
        if !comparefn.is_undefined() {
            let args: smallvec::SmallVec<[Value; 8]> = smallvec::smallvec![x, y];
            let r = self.run_callable_sync(context, &comparefn, Value::undefined(), args)?;
            let n = self.coerce_to_number(context, &r)?;
            let f = n.as_f64();
            return Ok(if f.is_nan() {
                Ordering::Equal
            } else if f < 0.0 {
                Ordering::Less
            } else if f > 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            });
        }
        let xs = self.coerce_to_string(context, &x)?;
        let ys = self.coerce_to_string(context, &y)?;
        Ok(xs.cmp(&ys))
    }

    /// Stable merge sort over `items`, propagating an abrupt completion
    /// from the comparator (Rust's `sort_by` cannot carry a `Result`).
    fn sort_merge(
        &mut self,
        context: &ExecutionContext,
        items: Vec<Value>,
        comparefn: Value,
    ) -> Result<Vec<Value>, VmError> {
        use std::cmp::Ordering;
        let n = items.len();
        if n <= 1 {
            return Ok(items);
        }
        let mid = n / 2;
        let mut left = items;
        let right = left.split_off(mid);
        let left = self.sort_merge(context, left, comparefn)?;
        let right = self.sort_merge(context, right, comparefn)?;
        let mut out = Vec::with_capacity(n);
        let (mut i, mut j) = (0usize, 0usize);
        while i < left.len() && j < right.len() {
            // Stable: keep the left element on a tie.
            if self.sort_compare(context, left[i], right[j], comparefn)? != Ordering::Greater {
                out.push(left[i]);
                i += 1;
            } else {
                out.push(right[j]);
                j += 1;
            }
        }
        out.extend_from_slice(&left[i..]);
        out.extend_from_slice(&right[j..]);
        Ok(out)
    }

    /// §23.1.3.30 live `Array.prototype.sort` over a generic receiver.
    pub(crate) fn array_sort(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        comparefn: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        // §23.1.3.30 step 1 — comparefn must be undefined or callable.
        if !comparefn.is_undefined() && !self.is_callable_runtime(&comparefn) {
            return Err(VmError::TypeError {
                message: "Array.prototype.sort comparator is not a function".to_string(),
            });
        }
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let cap = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        // SortIndexedProperties: collect the present indexed values.
        let mut items: Vec<Value> = Vec::new();
        for k in 0..cap {
            let key = k.to_string();
            if self.ordinary_has_property_value(
                context,
                o,
                &crate::VmPropertyKey::String(&key),
                0,
            )? {
                items.push(self.get_property_value_for_call(context, o, &key)?);
            }
        }
        let item_count = items.len();
        let sorted = self.sort_merge(context, items, comparefn)?;
        // Write the sorted prefix back, then delete the trailing holes.
        for (j, item) in sorted.into_iter().enumerate() {
            let key = j.to_string();
            self.ordinary_set_data_value(
                context,
                o,
                &crate::VmPropertyKey::String(&key),
                item,
                o,
                0,
            )?;
        }
        for k in item_count..cap {
            let key = k.to_string();
            self.ordinary_delete_value(context, o, &crate::VmPropertyKey::String(&key), 0)?;
        }
        Ok(o)
    }

    /// `Set(O, "length", v, true)` — a `false` result (non-writable /
    /// frozen length) raises the spec TypeError.
    fn array_set_length_throwing(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        len: f64,
    ) -> Result<(), VmError> {
        if let Some(arr) = o.as_array() {
            if !crate::array::length_writable(arr, self.gc_heap()) {
                return Err(VmError::TypeError {
                    message: "Cannot assign to read only property 'length' of object".to_string(),
                });
            }
        } else {
            return self.array_set_property_throwing(
                context,
                o,
                "length",
                Value::number(NumberValue::from_f64(len)),
            );
        }
        let ok = self.ordinary_set_data_value(
            context,
            o,
            &crate::VmPropertyKey::String("length"),
            Value::number(NumberValue::from_f64(len)),
            o,
            0,
        )?;
        if ok {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: "Cannot assign to read only property 'length' of object".to_string(),
            })
        }
    }

    pub(crate) fn array_set_property_throwing(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
        value: Value,
    ) -> Result<(), VmError> {
        if let Some(arr) = o.as_array()
            && crate::object::array_index_property_name(key).is_some()
            && crate::array::get_named_property(arr, self.gc_heap(), key).is_none()
        {
            let proto = self.constructor_prototype_value("Array")?;
            if let Some(proto) = proto.as_object() {
                match crate::object::resolve_set(proto, self.gc_heap(), key) {
                    crate::object::SetOutcome::InvokeSetter { setter } => {
                        let args: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                        self.run_callable_sync(context, &setter, o, args)?;
                        return Ok(());
                    }
                    crate::object::SetOutcome::Reject { .. } => {
                        return Err(VmError::TypeError {
                            message: format!("Cannot assign to property '{key}'"),
                        });
                    }
                    crate::object::SetOutcome::AssignData => {}
                }
            }
        }
        if let Some(obj) = o.as_object() {
            match crate::object::resolve_set(obj, self.gc_heap(), key) {
                crate::object::SetOutcome::InvokeSetter { setter } => {
                    let args: SmallVec<[Value; 8]> = smallvec::smallvec![value];
                    self.run_callable_sync(context, &setter, o, args)?;
                    return Ok(());
                }
                crate::object::SetOutcome::Reject { .. } => {
                    return Err(VmError::TypeError {
                        message: format!("Cannot assign to property '{key}'"),
                    });
                }
                crate::object::SetOutcome::AssignData => {}
            }
        }
        let ok = self.ordinary_set_data_value(
            context,
            o,
            &crate::VmPropertyKey::String(key),
            value,
            o,
            0,
        )?;
        if ok {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: format!("Cannot assign to read only property '{key}' of object"),
            })
        }
    }

    fn array_delete_property_throwing(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
    ) -> Result<(), VmError> {
        let deleted =
            self.ordinary_delete_value(context, o, &crate::VmPropertyKey::String(key), 0)?;
        if deleted {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: format!("Cannot delete property '{key}'"),
            })
        }
    }

    fn array_relative_index(
        &mut self,
        context: &ExecutionContext,
        arg: Option<&Value>,
        default: f64,
        len: usize,
    ) -> Result<usize, VmError> {
        let n = match arg {
            None => default,
            Some(v) if v.is_undefined() => default,
            Some(v) => {
                let n = self.coerce_to_number(context, v)?.as_f64();
                if n.is_nan() {
                    0.0
                } else if n.is_infinite() {
                    n
                } else {
                    n.trunc()
                }
            }
        };
        if n == f64::NEG_INFINITY {
            return Ok(0);
        }
        if n < 0.0 {
            Ok(((len as f64) + n).max(0.0) as usize)
        } else {
            Ok(n.min(len as f64) as usize)
        }
    }

    fn array_clamped_count(
        &mut self,
        context: &ExecutionContext,
        arg: &Value,
        max: usize,
    ) -> Result<usize, VmError> {
        let n = self.coerce_to_number(context, arg)?.as_f64();
        if n.is_nan() || n <= 0.0 {
            return Ok(0);
        }
        if n.is_infinite() {
            return Ok(max);
        }
        Ok((n.trunc() as usize).min(max))
    }

    /// §23.1.3.23 live `Array.prototype.push` over a generic array-like receiver.
    /// §10.4.2 OrdinarySet for an array index / named own key with the
    /// array itself as the receiver. Unlike the dense write fast path it
    /// honours an inherited accessor setter on the prototype chain, an
    /// own non-writable property, and a non-extensible array — returning
    /// `false` so a `Set(..., Throw=true)` caller (e.g. `push`) can raise
    /// the spec `TypeError`.
    fn array_ordinary_set_own(
        &mut self,
        context: &ExecutionContext,
        arr: crate::array::JsArray,
        key: &str,
        value: Value,
    ) -> Result<bool, VmError> {
        if let Some((_getter, setter)) = crate::array::get_accessor(arr, self.gc_heap(), key) {
            return match setter {
                Some(s) if crate::abstract_ops::is_callable(&s) => {
                    let mut a: SmallVec<[Value; 8]> = SmallVec::new();
                    a.push(value);
                    self.run_callable_sync(context, &s, Value::array(arr), a)?;
                    Ok(true)
                }
                _ => Ok(false),
            };
        }
        let idx = object::array_index_property_name(key).map(|i| i as usize);
        let has_own = match idx {
            Some(i) => crate::array::has_own_element(arr, self.gc_heap(), i),
            None => crate::array::get_named_property(arr, self.gc_heap(), key).is_some(),
        };
        if has_own {
            let writable = crate::array::get_property_flags(arr, self.gc_heap(), key)
                .is_none_or(|f| f.writable());
            if !writable {
                return Ok(false);
            }
        } else {
            // Absent own — consult the prototype chain for an inherited
            // setter or non-writable shadow before installing a new slot.
            let proto = self.constructor_prototype_value("Array")?;
            if let Some(proto_obj) = proto.as_object() {
                match crate::object::resolve_set(proto_obj, self.gc_heap(), key) {
                    crate::object::SetOutcome::InvokeSetter { setter } => {
                        let mut a: SmallVec<[Value; 8]> = SmallVec::new();
                        a.push(value);
                        self.run_callable_sync(context, &setter, Value::array(arr), a)?;
                        return Ok(true);
                    }
                    crate::object::SetOutcome::Reject { .. } => return Ok(false),
                    crate::object::SetOutcome::AssignData => {}
                }
            }
            if !crate::array::is_extensible(arr, self.gc_heap()) {
                return Ok(false);
            }
        }
        match idx {
            Some(i) => crate::array::define_index_value(arr, self.gc_heap_mut(), i, value)
                .map_err(|_| VmError::TypeMismatch)?,
            None => {
                crate::array::set_named_property(arr, self.gc_heap_mut(), key, value)
                    .map_err(|_| VmError::TypeMismatch)?;
            }
        }
        Ok(true)
    }

    pub(crate) fn array_push(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)? as f64;
        let arg_count = args.len() as f64;
        // §23.1.3.23 step 4 — len + argCount must stay a safe integer.
        if len + arg_count > 9_007_199_254_740_991.0 {
            return Err(VmError::TypeError {
                message: "Pushing too many elements onto an array-like".to_string(),
            });
        }
        let mut n = len;
        for &arg in args {
            let key = format_index_key(n);
            // §23.1.3.23 step 6.c — Set(O, ToString(n), E, true): a real
            // array honours inherited setters / writability and throws on
            // failure; a generic array-like routes through the throwing
            // property setter.
            if let Some(arr) = o.as_array() {
                if !self.array_ordinary_set_own(context, arr, &key, arg)? {
                    return Err(VmError::TypeError {
                        message: format!("Cannot assign to read only property '{key}'"),
                    });
                }
            } else {
                self.array_set_property_throwing(context, o, &key, arg)?;
            }
            n += 1.0;
        }
        self.array_set_length_throwing(context, o, n)?;
        Ok(Value::number(NumberValue::from_f64(n)))
    }

    /// §23.1.3.21 live `Array.prototype.pop` over a generic array-like receiver.
    pub(crate) fn array_pop(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)? as f64;
        if len == 0.0 {
            self.array_set_length_throwing(context, o, 0.0)?;
            return Ok(Value::undefined());
        }
        let new_len = len - 1.0;
        let key = format_index_key(new_len);
        let element = self.get_property_value_for_call(context, o, &key)?;
        let deleted =
            self.ordinary_delete_value(context, o, &crate::VmPropertyKey::String(&key), 0)?;
        if !deleted {
            return Err(VmError::TypeError {
                message: format!("Cannot delete property '{key}'"),
            });
        }
        self.array_set_length_throwing(context, o, new_len)?;
        Ok(element)
    }

    /// §23.1.3.26 live `Array.prototype.shift`.
    pub(crate) fn array_shift(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        if len == 0 {
            self.array_set_length_throwing(context, o, 0.0)?;
            return Ok(Value::undefined());
        }
        let first = self.get_property_value_for_call(context, o, "0")?;
        let scan_len = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        for k in 1..scan_len {
            let from = k.to_string();
            let to = (k - 1).to_string();
            let has = self.ordinary_has_property_value(
                context,
                o,
                &crate::VmPropertyKey::String(&from),
                0,
            )?;
            if has {
                let value = self.get_property_value_for_call(context, o, &from)?;
                self.array_set_property_throwing(context, o, &to, value)?;
            } else {
                self.array_delete_property_throwing(context, o, &to)?;
            }
        }
        let tail = format_index_key((len - 1) as f64);
        self.array_delete_property_throwing(context, o, &tail)?;
        self.array_set_length_throwing(context, o, (len - 1) as f64)?;
        Ok(first)
    }

    /// §23.1.3.34 live `Array.prototype.unshift`.
    pub(crate) fn array_unshift(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let arg_count = args.len();
        if arg_count == 0 {
            self.array_set_length_throwing(context, o, len as f64)?;
            return Ok(Value::number(NumberValue::from_f64(len as f64)));
        }
        let new_len = len as f64 + arg_count as f64;
        if new_len > 9_007_199_254_740_991.0 {
            return Err(VmError::TypeError {
                message: "Unshifting too many elements onto an array-like".to_string(),
            });
        }

        if len <= MAX_ARRAY_LIKE_PROBE_LEN {
            for k in (0..len).rev() {
                self.unshift_move_index(context, o, k, arg_count)?;
            }
        } else {
            let mut candidates = self.unshift_sparse_candidates(o, len, arg_count)?;
            while let Some(k) = candidates.pop_last() {
                self.unshift_move_index(context, o, k, arg_count)?;
            }
        }

        for (j, value) in args.iter().enumerate() {
            self.array_set_property_throwing(context, o, &j.to_string(), *value)?;
        }
        self.array_set_length_throwing(context, o, new_len)?;
        Ok(Value::number(NumberValue::from_f64(new_len)))
    }

    fn unshift_move_index(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        from_index: usize,
        arg_count: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        let to = format_index_key((from_index + arg_count) as f64);
        let has =
            self.ordinary_has_property_value(context, o, &crate::VmPropertyKey::String(&from), 0)?;
        if has {
            let value = self.get_property_value_for_call(context, o, &from)?;
            self.array_set_property_throwing(context, o, &to, value)
        } else {
            self.array_delete_property_throwing(context, o, &to)
        }
    }

    fn unshift_sparse_candidates(
        &mut self,
        o: Value,
        len: usize,
        arg_count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len.saturating_add(arg_count), &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        let mut candidates = std::collections::BTreeSet::new();
        for index in indices {
            if index < len {
                candidates.insert(index);
            }
            if index >= arg_count {
                let from = index - arg_count;
                if from < len {
                    candidates.insert(from);
                }
            }
        }
        Ok(candidates)
    }

    /// §23.1.3.4 live `Array.prototype.copyWithin`.
    pub(crate) fn array_copy_within(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let to = self.array_relative_index(context, args.first(), 0.0, len)?;
        let from = self.array_relative_index(context, args.get(1), 0.0, len)?;
        let final_index = self.array_relative_index(context, args.get(2), len as f64, len)?;
        let count = final_index.saturating_sub(from).min(len.saturating_sub(to));
        if count == 0 {
            return Ok(o);
        }

        let backwards = from < to && to < from.saturating_add(count);
        if count <= MAX_ARRAY_LIKE_PROBE_LEN {
            if backwards {
                for offset in (0..count).rev() {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            } else {
                for offset in 0..count {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            }
        } else {
            let mut offsets = self.copy_within_sparse_offsets(o, len, from, to, count)?;
            if backwards {
                while let Some(offset) = offsets.pop_last() {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            } else {
                for offset in offsets {
                    self.copy_within_move_index(context, o, from + offset, to + offset)?;
                }
            }
        }
        Ok(o)
    }

    fn copy_within_move_index(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        let to = format_index_key(to_index as f64);
        let has = self.array_method_has_property(context, o, &from)?;
        if has {
            let value = self.array_method_get_property(context, o, &from)?;
            self.array_set_property_throwing(context, o, &to, value)
        } else {
            self.array_delete_property_throwing(context, o, &to)
        }
    }

    fn array_method_has_property(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
    ) -> Result<bool, VmError> {
        let property_key = crate::VmPropertyKey::String(key);
        if self.ordinary_has_property_value(context, o, &property_key, 0)? {
            return Ok(true);
        }
        if o.is_array() {
            let proto = self.get_prototype_for_op(&o)?;
            if !proto.is_nullish() {
                return self.ordinary_has_property_value(context, proto, &property_key, 0);
            }
        }
        Ok(false)
    }

    fn array_method_get_property(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        key: &str,
    ) -> Result<Value, VmError> {
        if let Some(arr) = o.as_array() {
            if let Some((getter, _setter)) = crate::array::get_accessor(arr, self.gc_heap(), key) {
                return match getter {
                    Some(getter) if crate::abstract_ops::is_callable(&getter) => {
                        let args: SmallVec<[Value; 8]> = SmallVec::new();
                        self.run_callable_sync(context, &getter, o, args)
                    }
                    _ => Ok(Value::undefined()),
                };
            }
            if let Some(idx) = crate::object::array_index_property_name(key)
                && crate::array::has_own_element(arr, self.gc_heap(), idx as usize)
            {
                return Ok(crate::array::get(arr, self.gc_heap(), idx as usize));
            }
            if let Some(value) = crate::array::get_named_property(arr, self.gc_heap(), key) {
                return Ok(value);
            }
            let proto = self.get_prototype_for_op(&o)?;
            if !proto.is_nullish()
                && self.ordinary_has_property_value(
                    context,
                    proto,
                    &crate::VmPropertyKey::String(key),
                    0,
                )?
            {
                return self.get_property_value_for_call(context, proto, key);
            }
        }
        self.get_property_value_for_call(context, o, key)
    }

    fn copy_within_sparse_offsets(
        &mut self,
        o: Value,
        len: usize,
        from: usize,
        to: usize,
        count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }

        let mut offsets = std::collections::BTreeSet::new();
        for index in indices {
            if index >= from && index < from.saturating_add(count) {
                offsets.insert(index - from);
            }
            if index >= to && index < to.saturating_add(count) {
                offsets.insert(index - to);
            }
        }
        Ok(offsets)
    }

    /// §23.1.3.28 live `Array.prototype.slice`.
    pub(crate) fn array_slice(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let start = self.array_relative_index(context, args.first(), 0.0, len)?;
        let final_index = self.array_relative_index(context, args.get(1), len as f64, len)?;
        let count = final_index.saturating_sub(start);
        let a = self.array_species_create(context, o, count, roots)?;
        if count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for n in 0..count {
                self.slice_copy_index(context, o, a, start + n, n)?;
            }
        } else {
            for n in self.slice_sparse_offsets(o, len, start, count)? {
                self.slice_copy_index(context, o, a, start + n, n)?;
            }
        }
        self.array_set_property_throwing(
            context,
            a,
            "length",
            Value::number(NumberValue::from_f64(count as f64)),
        )?;
        Ok(a)
    }

    fn slice_copy_index(
        &mut self,
        context: &ExecutionContext,
        from_object: Value,
        to_object: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        if !self.array_method_has_property(context, from_object, &from)? {
            return Ok(());
        }
        let value = self.array_method_get_property(context, from_object, &from)?;
        let to = format_index_key(to_index as f64);
        self.create_data_property_or_throw(context, to_object, &to, value)
    }

    fn array_species_create(
        &mut self,
        context: &ExecutionContext,
        original: Value,
        length: usize,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if !self.array_species_is_array(context, original)? {
            return self.array_create_with_length(original, length, roots);
        }
        let default_ctor = crate::object::get(self.global_this, &self.gc_heap, "Array")
            .ok_or_else(|| VmError::TypeError {
                message: "%Array% intrinsic is missing".to_string(),
            })?;
        let constructor = self.species_constructor_value(context, &original, &default_ctor)?;
        if crate::abstract_ops::same_value(&constructor, &default_ctor, &self.gc_heap) {
            return self.array_create_with_length(original, length, roots);
        }
        let argv: SmallVec<[Value; 8]> =
            smallvec::smallvec![Value::number(NumberValue::from_f64(length as f64))];
        self.run_construct_sync(context, &constructor, constructor, argv)
    }

    fn array_create_with_length(
        &mut self,
        receiver_root: Value,
        length: usize,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if length > u32::MAX as usize {
            return Err(VmError::RangeError {
                message: "Invalid array length".to_string(),
            });
        }
        let mut external_visit = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            receiver_root.trace_value_slots(visit);
            for root in roots {
                for value in *root {
                    value.trace_value_slots(visit);
                }
            }
        };
        let arr = crate::array::alloc_array_with_roots(&mut self.gc_heap, &mut external_visit)
            .map_err(|_| VmError::RangeError {
                message: "Invalid array length".to_string(),
            })?;
        crate::array::set_length(arr, &mut self.gc_heap, length).map_err(|_| {
            VmError::RangeError {
                message: "Invalid array length".to_string(),
            }
        })?;
        Ok(Value::array(arr))
    }

    fn array_species_is_array(
        &self,
        _context: &ExecutionContext,
        original: Value,
    ) -> Result<bool, VmError> {
        let mut current = original;
        let mut hops = 0usize;
        loop {
            if current.is_array() {
                return Ok(true);
            }
            let Some(proxy) = current.as_proxy() else {
                return Ok(false);
            };
            if proxy.is_revoked(&self.gc_heap) {
                return Err(VmError::TypeError {
                    message: "Cannot perform IsArray on a proxy that has been revoked".to_string(),
                });
            }
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                return Ok(false);
            }
            current = proxy.target(&self.gc_heap);
            hops += 1;
        }
    }

    pub(crate) fn create_data_property_or_throw(
        &mut self,
        context: &ExecutionContext,
        target: Value,
        key: &str,
        value: Value,
    ) -> Result<(), VmError> {
        let descriptor = PartialPropertyDescriptor {
            value: Some(value),
            writable: Some(true),
            enumerable: Some(true),
            configurable: Some(true),
            ..Default::default()
        };
        let ok = self.define_own_property_value(
            context,
            &target,
            &crate::VmPropertyKey::String(key),
            descriptor,
        )?;
        if ok {
            Ok(())
        } else {
            Err(VmError::TypeError {
                message: format!("Cannot create property '{key}'"),
            })
        }
    }

    fn slice_sparse_offsets(
        &mut self,
        o: Value,
        len: usize,
        start: usize,
        count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }

        let mut offsets = std::collections::BTreeSet::new();
        for index in indices {
            if index >= start && index < start.saturating_add(count) {
                offsets.insert(index - start);
            }
        }
        Ok(offsets)
    }

    /// §23.1.3.31 live `Array.prototype.splice`.
    pub(crate) fn array_splice(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let actual_start = if args.is_empty() {
            0
        } else {
            self.array_relative_index(context, args.first(), 0.0, len)?
        };
        let insert_count = args.len().saturating_sub(2);
        let actual_delete_count = match args.len() {
            0 => 0,
            1 => len.saturating_sub(actual_start),
            _ => self.array_clamped_count(context, &args[1], len.saturating_sub(actual_start))?,
        };
        let new_len = len
            .checked_sub(actual_delete_count)
            .and_then(|n| n.checked_add(insert_count))
            .ok_or_else(|| VmError::TypeError {
                message: "Invalid array length".to_string(),
            })?;
        if new_len > 9_007_199_254_740_991usize {
            return Err(VmError::TypeError {
                message: "Invalid array length".to_string(),
            });
        }

        let removed = self.array_species_create(context, o, actual_delete_count, roots)?;
        if actual_delete_count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for n in 0..actual_delete_count {
                self.splice_copy_deleted_index(context, o, removed, actual_start + n, n)?;
            }
        } else {
            for n in self.splice_sparse_offsets(o, len, actual_start, actual_delete_count)? {
                self.splice_copy_deleted_index(context, o, removed, actual_start + n, n)?;
            }
        }
        self.array_set_property_throwing(
            context,
            removed,
            "length",
            Value::number(NumberValue::from_f64(actual_delete_count as f64)),
        )?;

        if insert_count < actual_delete_count {
            self.splice_shift_left(
                context,
                o,
                len,
                actual_start,
                actual_delete_count,
                insert_count,
            )?;
        } else if insert_count > actual_delete_count {
            self.splice_shift_right(
                context,
                o,
                len,
                actual_start,
                actual_delete_count,
                insert_count,
            )?;
        }

        for (offset, value) in args.iter().skip(2).copied().enumerate() {
            let key = format_index_key((actual_start + offset) as f64);
            self.array_set_property_throwing(context, o, &key, value)?;
        }
        self.array_set_length_throwing(context, o, new_len as f64)?;
        Ok(removed)
    }

    fn splice_copy_deleted_index(
        &mut self,
        context: &ExecutionContext,
        from_object: Value,
        to_object: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        if !self.array_method_has_property(context, from_object, &from)? {
            return Ok(());
        }
        let value = self.array_method_get_property(context, from_object, &from)?;
        let to = format_index_key(to_index as f64);
        self.create_data_property_or_throw(context, to_object, &to, value)
    }

    fn splice_shift_left(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        len: usize,
        actual_start: usize,
        actual_delete_count: usize,
        insert_count: usize,
    ) -> Result<(), VmError> {
        let shift = actual_delete_count - insert_count;
        let tail_count = len.saturating_sub(actual_start + actual_delete_count);
        if tail_count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for k in actual_start..len.saturating_sub(actual_delete_count) {
                self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
            }
            for k in (len - shift)..len {
                let key = format_index_key(k as f64);
                self.array_delete_property_throwing(context, o, &key)?;
            }
            return Ok(());
        }

        let candidates =
            self.splice_shift_candidates(o, len, actual_start, actual_delete_count, insert_count)?;
        for k in candidates {
            self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
        }
        let own_indices = self.splice_own_indices(o, len)?;
        for k in own_indices.range((len - shift)..len) {
            let key = format_index_key(*k as f64);
            self.array_delete_property_throwing(context, o, &key)?;
        }
        Ok(())
    }

    fn splice_shift_right(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        len: usize,
        actual_start: usize,
        actual_delete_count: usize,
        insert_count: usize,
    ) -> Result<(), VmError> {
        let tail_count = len.saturating_sub(actual_start + actual_delete_count);
        if tail_count <= MAX_ARRAY_LIKE_PROBE_LEN {
            for k in (actual_start..len.saturating_sub(actual_delete_count)).rev() {
                self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
            }
            return Ok(());
        }

        let candidates =
            self.splice_shift_candidates(o, len, actual_start, actual_delete_count, insert_count)?;
        for k in candidates.into_iter().rev() {
            self.splice_move_or_delete(context, o, k + actual_delete_count, k + insert_count)?;
        }
        Ok(())
    }

    fn splice_move_or_delete(
        &mut self,
        context: &ExecutionContext,
        o: Value,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), VmError> {
        let from = format_index_key(from_index as f64);
        let to = format_index_key(to_index as f64);
        if self.array_method_has_property(context, o, &from)? {
            let value = self.array_method_get_property(context, o, &from)?;
            self.array_set_property_throwing(context, o, &to, value)
        } else {
            self.array_delete_property_throwing(context, o, &to)
        }
    }

    fn splice_sparse_offsets(
        &mut self,
        o: Value,
        len: usize,
        start: usize,
        count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut offsets = std::collections::BTreeSet::new();
        offsets.extend(0..count.min(MAX_SPARSE_PREFIX_PROBE_LEN));
        for index in self.splice_chain_indices(o, len)? {
            if index >= start && index < start.saturating_add(count) {
                offsets.insert(index - start);
            }
        }
        Ok(offsets)
    }

    fn splice_shift_candidates(
        &mut self,
        o: Value,
        len: usize,
        actual_start: usize,
        actual_delete_count: usize,
        insert_count: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut candidates = std::collections::BTreeSet::new();
        let tail_start = actual_start + actual_delete_count;
        let tail_end = len;
        let target_start = actual_start + insert_count;
        let target_end = len - actual_delete_count + insert_count;
        for index in self.splice_chain_indices(o, len.max(target_end))? {
            if index >= tail_start && index < tail_end {
                candidates.insert(index - actual_delete_count);
            }
            if index >= target_start && index < target_end {
                candidates.insert(index - insert_count);
            }
        }
        candidates.retain(|k| *k >= actual_start && *k < len.saturating_sub(actual_delete_count));
        Ok(candidates)
    }

    fn splice_chain_indices(
        &mut self,
        o: Value,
        len: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        let mut current = o;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(self, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = self.get_prototype_for_op(&current)?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        Ok(indices)
    }

    fn splice_own_indices(
        &self,
        o: Value,
        len: usize,
    ) -> Result<std::collections::BTreeSet<usize>, VmError> {
        let mut indices = std::collections::BTreeSet::new();
        collect_own_indices_below(self, &o, len, &mut indices);
        Ok(indices)
    }

    /// §23.1.3.39 live `Array.prototype.toReversed`.
    pub(crate) fn array_to_reversed(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        self.ensure_change_by_copy_len(len)?;
        let mut out = Vec::with_capacity(len);
        for k in 0..len {
            let from = format_index_key((len - k - 1) as f64);
            out.push(self.array_method_get_property(context, o, &from)?);
        }
        self.array_create_from_dense_values(out)
    }

    /// §23.1.3.40 live `Array.prototype.toSpliced`.
    pub(crate) fn array_to_spliced(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        args: &[Value],
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        let actual_start = self.array_relative_index(context, args.first(), 0.0, len)?;
        let insert_count = args.len().saturating_sub(2);
        let skip_count = match args.len() {
            0 => 0,
            1 => len.saturating_sub(actual_start),
            _ => self.array_clamped_count(context, &args[1], len.saturating_sub(actual_start))?,
        };
        let new_len = len
            .checked_sub(skip_count)
            .and_then(|n| n.checked_add(insert_count))
            .ok_or_else(|| VmError::TypeError {
                message: "Invalid array length".to_string(),
            })?;
        if new_len > MAX_SAFE_ARRAY_LENGTH {
            return Err(VmError::TypeError {
                message: "Invalid array length".to_string(),
            });
        }
        self.ensure_change_by_copy_len(new_len)?;

        let mut out = Vec::with_capacity(new_len);
        for k in 0..actual_start {
            out.push(self.array_method_get_property(context, o, &format_index_key(k as f64))?);
        }
        out.extend(args.iter().skip(2).copied());
        for k in (actual_start + skip_count)..len {
            out.push(self.array_method_get_property(context, o, &format_index_key(k as f64))?);
        }
        self.array_create_from_dense_values(out)
    }

    /// §23.1.3.41 live `Array.prototype.toSorted`.
    pub(crate) fn array_to_sorted(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        comparefn: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        if !comparefn.is_undefined() && !self.is_callable_runtime(&comparefn) {
            return Err(VmError::TypeError {
                message: "Array.prototype.toSorted comparator is not a function".to_string(),
            });
        }
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        self.ensure_change_by_copy_len(len)?;
        let mut items = Vec::with_capacity(len);
        for k in 0..len {
            items.push(self.array_method_get_property(context, o, &format_index_key(k as f64))?);
        }
        let sorted = self.sort_merge(context, items, comparefn)?;
        self.array_create_from_dense_values(sorted)
    }

    /// §23.1.3.42 live `Array.prototype.with`.
    pub(crate) fn array_with(
        &mut self,
        context: &ExecutionContext,
        receiver: Value,
        index: Value,
        value: Value,
        roots: &[&[Value]],
    ) -> Result<Value, VmError> {
        let o = if receiver.is_object_type() {
            receiver
        } else {
            self.box_sloppy_this_primitive_runtime_rooted(receiver, roots)?
        };
        let len = length_of_array_like(self, context, &o)?;
        self.ensure_change_by_copy_len(len)?;
        let actual = self.array_relative_index_strict(context, &index, len)?;
        let mut out = Vec::with_capacity(len);
        for k in 0..len {
            if k == actual {
                out.push(value);
            } else {
                out.push(self.array_method_get_property(
                    context,
                    o,
                    &format_index_key(k as f64),
                )?);
            }
        }
        self.array_create_from_dense_values(out)
    }

    fn ensure_change_by_copy_len(&self, len: usize) -> Result<(), VmError> {
        if len > u32::MAX as usize || len > MAX_ARRAY_LIKE_PROBE_LEN {
            return Err(VmError::RangeError {
                message: "Invalid array length".to_string(),
            });
        }
        Ok(())
    }

    fn array_relative_index_strict(
        &mut self,
        context: &ExecutionContext,
        arg: &Value,
        len: usize,
    ) -> Result<usize, VmError> {
        let n = self.coerce_to_number(context, arg)?.as_f64();
        let relative = if n.is_nan() {
            0.0
        } else if n.is_infinite() {
            n
        } else {
            n.trunc()
        };
        let actual = if relative < 0.0 {
            len as f64 + relative
        } else {
            relative
        };
        if !actual.is_finite() || actual < 0.0 || actual >= len as f64 {
            return Err(VmError::RangeError {
                message: "index out of range".to_string(),
            });
        }
        Ok(actual as usize)
    }

    pub(crate) fn array_create_from_dense_values(
        &mut self,
        values: Vec<Value>,
    ) -> Result<Value, VmError> {
        if values.len() > u32::MAX as usize {
            return Err(VmError::RangeError {
                message: "Invalid array length".to_string(),
            });
        }
        let len = values.len();
        let heap = self.gc_heap_mut();
        let mut visitor = |visit: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            for value in &values {
                value.trace_value_slots(visit);
            }
        };
        let arr = crate::array::alloc_array_with_roots(heap, &mut visitor).map_err(|_| {
            VmError::RangeError {
                message: "Invalid array length".to_string(),
            }
        })?;
        crate::array::with_elements_mut(arr, heap, |elements| {
            elements.extend(values);
        });
        crate::array::set_length(arr, heap, len).map_err(|_| VmError::RangeError {
            message: "Invalid array length".to_string(),
        })?;
        Ok(Value::array(arr))
    }
}

/// Format an array index that may exceed `u32` (`length` runs to
/// `2**53 - 1`) as its canonical decimal string for use as a property
/// key, avoiding the float exponent form `to_string` would produce.
fn format_index_key(n: f64) -> String {
    if (0.0..9_007_199_254_740_992.0).contains(&n) && n.fract() == 0.0 {
        (n as u64).to_string()
    } else {
        crate::number::NumberValue::from_f64(n).to_display_string()
    }
}

/// Add the own indexed keys (`< len`) of a single value to `indices`.
/// Covers dense arrays (non-hole element positions), string primitives
/// / wrappers (code-unit indices), and ordinary objects (numeric keys
/// in the property bag). Does not walk the prototype chain.
fn collect_own_indices_below(
    interp: &Interpreter,
    value: &Value,
    len: usize,
    indices: &mut std::collections::BTreeSet<usize>,
) {
    let heap = interp.gc_heap();
    if let Some(arr) = value.as_array() {
        let alen = crate::array::len(arr, heap).min(len);
        crate::array::with_elements(arr, heap, |els| {
            for (i, v) in els.iter().enumerate().take(alen) {
                if !v.is_hole() {
                    indices.insert(i);
                }
            }
        });
        return;
    }
    if let Some(obj) = value.as_object() {
        if let Some(s) = crate::object::string_data(obj, heap) {
            for i in 0..(s.len() as usize).min(len) {
                indices.insert(i);
            }
        }
        crate::object::with_properties(obj, heap, |p| {
            for k in p.keys() {
                if let Ok(i) = k.parse::<usize>()
                    && i < len
                {
                    indices.insert(i);
                }
            }
        });
        return;
    }
    if let Some(s) = value.as_string(heap) {
        for i in 0..(s.len() as usize).min(len) {
            indices.insert(i);
        }
    }
}

/// Dispatches callback-based `Array.prototype` NativeCtx methods.
pub(crate) fn array_callback_native_dispatch(
    name: &str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let raw_receiver = *ctx.this_value();
    if raw_receiver.is_null() || raw_receiver.is_undefined() {
        return Err(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: "Array.prototype method called on null or undefined".to_string(),
        });
    }
    let callback = args.first().cloned().unwrap_or(Value::undefined());
    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
    let (interp, ctx_opt) = ctx.interp_mut_and_context();
    let context = ctx_opt.ok_or(NativeError::TypeError {
        name: "Array.prototype callback",
        reason: "missing execution context".to_string(),
    })?;
    // §22.1.3 step 1 — `O = ? ToObject(this value)`. Box primitive
    // receivers so the callback's `O` argument and the prototype-chain
    // walk see a real wrapper (e.g. `Boolean.prototype[k]` inherited
    // indices).
    let receiver = if raw_receiver.is_object_type() {
        raw_receiver
    } else {
        interp
            .box_sloppy_this_primitive_runtime_rooted(raw_receiver, &[args])
            .map_err(|err| {
                crate::native_function::vm_to_native_error(err, "Array.prototype callback")
            })?
    };
    // §23.1.3.* step 2 — len = ? LengthOfArrayLike(O), read once via
    // `[[Get]]` (observes a `get length()`). The walk below is LIVE:
    // each index is re-checked with `HasProperty(O, k)` / `Get(O, k)`
    // during iteration, so a callback that mutates the receiver is
    // observed in spec order and a Function / exotic receiver's indexed
    // properties are seen (the previous one-shot snapshot saw neither).
    let len = length_of_array_like(interp, &context, &receiver).map_err(|err| {
        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
    })?;
    // §23.1.3.* step 3 — `if IsCallable(callbackfn) is false, throw a
    // TypeError`, ordered after `ToObject` + `LengthOfArrayLike`.
    if !interp.is_callable_runtime(&callback) {
        return Err(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: "callback is not a function".to_string(),
        });
    }
    let callback_roots = [receiver, callback, this_arg];
    let output_target = match name {
        "map" => Some(
            interp
                .array_species_create(&context, receiver, len, &[args, &callback_roots])
                .map_err(|err| {
                    crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                })?,
        ),
        "filter" | "flatMap" => Some(
            interp
                .array_species_create(&context, receiver, 0, &[args, &callback_roots])
                .map_err(|err| {
                    crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                })?,
        ),
        _ => None,
    };
    // §23.1.3.12 `flatMap` is FlattenIntoArray(A, O, len, 0, 1, mapper,
    // T): each callback result that IsArray is spliced one level deep
    // through the observable HasProperty / Get / CreateDataProperty
    // protocol — not the raw element snapshot the generic loop uses.
    if name == "flatMap" {
        let target = output_target.expect("flatMap output target created above");
        let anchor_base = interp.push_iteration_anchor(target) - 1;
        interp.push_iteration_anchor(receiver);
        let probe_len = len.min(MAX_ARRAY_LIKE_PROBE_LEN);
        let result = interp.flatten_into_array(
            &context,
            target,
            receiver,
            probe_len,
            0,
            1,
            Some((callback, this_arg)),
        );
        interp.pop_iteration_anchors_to(anchor_base);
        result.map_err(|err| crate::native_function::vm_to_native_error(err, "flatMap"))?;
        return Ok(target);
    }
    // `find` family visits every index `0..len` (an absent slot yields
    // `undefined` for the element); the rest skip absent indices.
    let visit_all = matches!(name, "find" | "findIndex" | "findLast" | "findLastIndex");
    let reverse = matches!(name, "reduceRight" | "findLast" | "findLastIndex");
    // `reduce` / `reduceRight` do not accept a `thisArg`; the callback
    // runs with `undefined` this (the second positional is the
    // initialValue, not a receiver).
    let cb_this = if name == "reduce" || name == "reduceRight" {
        Value::undefined()
    } else {
        this_arg
    };
    // String-exotic wrappers expose their code-unit indices through
    // `[[StringData]]`, which the ordinary `[[HasProperty]]` ladder may
    // not surface — resolve those directly.
    let string_data = receiver
        .as_object()
        .and_then(|o| crate::object::string_data(o, interp.gc_heap()));
    // Index visit order. A bounded `0..len` ladder is spec-exact for any
    // receiver (dense array, Function, object with getters, mutation
    // mid-walk). A pathological `length` (> MAX_ARRAY_LIKE_PROBE_LEN)
    // falls back to the sparse present-index set across the prototype
    // chain so the walk never runs billions of `HasProperty` probes.
    let index_iter: Box<dyn Iterator<Item = usize>> = if len <= MAX_ARRAY_LIKE_PROBE_LEN {
        if reverse {
            Box::new((0..len).rev())
        } else {
            Box::new(0..len)
        }
    } else {
        let mut indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut current = receiver;
        let mut hops = 0usize;
        loop {
            collect_own_indices_below(interp, &current, len, &mut indices);
            if hops >= object::PROTO_CHAIN_HARD_CAP {
                break;
            }
            let proto = interp.get_prototype_for_op(&current).map_err(|err| {
                crate::native_function::vm_to_native_error(err, "Array.prototype callback")
            })?;
            if proto.is_null() || !proto.is_object_type() {
                break;
            }
            current = proto;
            hops += 1;
        }
        let mut v: Vec<usize> = indices.into_iter().collect();
        if reverse {
            v.reverse();
        }
        Box::new(v.into_iter())
    };

    let mut acc = Value::undefined();
    let mut found_idx: Option<usize> = None;
    let mut found_val = Value::undefined();
    let mut bool_acc: bool = matches!(name, "every");
    let mut target_index = 0usize;
    let mut reduce_has_init = args.len() >= 2;
    if (name == "reduce" || name == "reduceRight") && reduce_has_init {
        acc = args[1];
    }
    for idx in index_iter {
        // Live `HasProperty(O, k)` + `Get(O, k)`. An absent index reads
        // as `(false, undefined)`; `find`-family methods visit it anyway.
        let (present, v) = if let Some(s) = string_data {
            match s.char_code_at(idx as u32, interp.gc_heap()) {
                Some(unit) => {
                    let ch =
                        crate::string::JsString::from_utf16_units(&[unit], interp.gc_heap_mut())
                            .map(Value::string)
                            .map_err(|_| NativeError::TypeError {
                                name: "Array.prototype callback",
                                reason: "out of memory".to_string(),
                            })?;
                    (true, ch)
                }
                None => (false, Value::undefined()),
            }
        } else if let Some(arr) = receiver.as_array() {
            // A present own element (data or accessor) reads through the
            // ordinary `[[Get]]`. An absent index (hole / beyond the
            // element store but `< len`) is not skipped outright:
            // §10.4.2.4 [[Get]] walks the Array.prototype chain, so an
            // inherited `Array.prototype[k]` is observed; a hole with no
            // inherited value reads as absent.
            let key = idx.to_string();
            let present = crate::array::has_own_element(arr, interp.gc_heap(), idx)
                || crate::array::get_accessor(arr, interp.gc_heap(), &key).is_some()
                || interp
                    .ordinary_has_property_value(
                        &context,
                        receiver,
                        &crate::VmPropertyKey::String(&key),
                        0,
                    )
                    .map_err(|err| {
                        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                    })?;
            if present {
                let v = interp
                    .get_property_value_for_call(&context, receiver, &key)
                    .map_err(|err| {
                        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                    })?;
                (true, v)
            } else {
                (false, Value::undefined())
            }
        } else {
            let key = idx.to_string();
            let has = interp
                .ordinary_has_property_value(
                    &context,
                    receiver,
                    &crate::VmPropertyKey::String(&key),
                    0,
                )
                .map_err(|err| {
                    crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                })?;
            if has {
                let v = interp
                    .get_property_value_for_call(&context, receiver, &key)
                    .map_err(|err| {
                        crate::native_function::vm_to_native_error(err, "Array.prototype callback")
                    })?;
                (true, v)
            } else {
                (false, Value::undefined())
            }
        };
        if !present && !visit_all {
            continue;
        }
        let cb_args: SmallVec<[Value; 8]> = match name {
            "reduce" | "reduceRight" => {
                if !reduce_has_init {
                    acc = v;
                    reduce_has_init = true;
                    continue;
                }
                smallvec::smallvec![acc, v, Value::number_f64(idx as f64), receiver,]
            }
            _ => smallvec::smallvec![v, Value::number_f64(idx as f64), receiver,],
        };
        let result = interp
            .run_callable_sync(&context, &callback, cb_this, cb_args)
            .map_err(|err| {
                crate::native_function::vm_to_native_error(err, "Array.prototype callback")
            })?;
        match name {
            "forEach" => {}
            "map" => {
                let target = output_target.ok_or(NativeError::TypeError {
                    name: "map",
                    reason: "missing output target".to_string(),
                })?;
                let key = format_index_key(idx as f64);
                interp
                    .create_data_property_or_throw(&context, target, &key, result)
                    .map_err(|err| crate::native_function::vm_to_native_error(err, "map"))?;
            }
            "filter" if result.to_boolean(interp.gc_heap()) => {
                let target = output_target.ok_or(NativeError::TypeError {
                    name: "filter",
                    reason: "missing output target".to_string(),
                })?;
                let key = format_index_key(target_index as f64);
                interp
                    .create_data_property_or_throw(&context, target, &key, v)
                    .map_err(|err| crate::native_function::vm_to_native_error(err, "filter"))?;
                target_index += 1;
            }
            "find" | "findLast" if result.to_boolean(interp.gc_heap()) => {
                found_val = v;
                found_idx = Some(idx);
                break;
            }
            "findIndex" | "findLastIndex" if result.to_boolean(interp.gc_heap()) => {
                found_idx = Some(idx);
                break;
            }
            "every" if !result.to_boolean(interp.gc_heap()) => {
                bool_acc = false;
                break;
            }
            "some" if result.to_boolean(interp.gc_heap()) => {
                bool_acc = true;
                break;
            }
            "reduce" | "reduceRight" => {
                acc = result;
            }
            _ => {}
        }
    }
    match name {
        "forEach" => Ok(Value::undefined()),
        "find" | "findLast" => Ok(found_val),
        "findIndex" | "findLastIndex" => Ok(Value::number(NumberValue::from_f64(
            found_idx.map_or(-1.0, |i| i as f64),
        ))),
        "every" | "some" => Ok(Value::boolean(bool_acc)),
        "reduce" | "reduceRight" => {
            if !reduce_has_init {
                return Err(NativeError::TypeError {
                    name: "reduce",
                    reason: "empty array with no initial value".to_string(),
                });
            }
            Ok(acc)
        }
        "map" | "filter" | "flatMap" => output_target.ok_or(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: "missing output target".to_string(),
        }),
        _ => Err(NativeError::TypeError {
            name: "Array.prototype callback",
            reason: format!("unknown callback method '{name}'"),
        }),
    }
}

fn native_for_each(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("forEach", ctx, args)
}
fn native_map(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("map", ctx, args)
}
fn native_filter(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("filter", ctx, args)
}
fn native_some(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("some", ctx, args)
}
fn native_every(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("every", ctx, args)
}
fn native_find(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("find", ctx, args)
}
fn native_find_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("findIndex", ctx, args)
}
fn native_find_last(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("findLast", ctx, args)
}
fn native_find_last_index(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("findLastIndex", ctx, args)
}
fn native_reduce(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("reduce", ctx, args)
}
fn native_reduce_right(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("reduceRight", ctx, args)
}
fn native_flat_map(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    array_callback_native_dispatch("flatMap", ctx, args)
}
