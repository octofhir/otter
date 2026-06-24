//! Constructor expression lowering.
//!
//! # Contents
//! - [`compile_new`] — lowers `new` expressions and constructor fast paths.
//!
//! # See also
//! - [`super`] — expression dispatch and shared helpers.

use crate::*;
use oxc_ast::ast::NewExpression;

pub(crate) fn compile_new(
    cx: &mut Compiler,
    new_expr: &NewExpression<'_>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let _ = span;
    let new_span = (new_expr.span.start, new_expr.span.end);
    let callee = unwrap_ts_expr(&new_expr.callee);
    // §13.3.5 — a `new C(...args)` whose arguments include a SpreadElement
    // must build its argument list dynamically. Route it straight to the
    // general `Op::NewSpread` path; the special-case constructor fast paths
    // below (Object / Array / Function / Intl, …) only accept positional
    // arguments and would otherwise reject the spread.
    if new_expr
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)))
    {
        let callee_reg = compile_expr(cx, callee, new_span)?;
        let args_reg = compile_spread_call_args(cx, &new_expr.arguments, new_span)?;
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::NewSpread,
            vec![
                Operand::Register(dst),
                Operand::Register(callee_reg),
                Operand::Register(args_reg),
            ],
            new_span,
        );
        return Ok(dst);
    }
    // ECMA-262 §19.3 / §20.5 native error constructors —
    // every one of `Error`, `TypeError`, `RangeError`,
    // `SyntaxError`, `ReferenceError`, `URIError`,
    // `EvalError` lowers to a dedicated opcode that
    // consults the per-interpreter [`ErrorClassRegistry`]
    // for the right prototype linkage.
    //
    // <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
    if let Expression::Identifier(id) = callee
        && cx.lookup_binding(id.name.as_str()).is_none()
        && find_module_import_binding(cx, id.name.as_str()).is_none()
        && is_builtin_error_class_name(id.name.as_str())
        && builtin_error_construct_fast_path_applies(id.name.as_str(), &new_expr.arguments)
    {
        return compile_builtin_error_construct(
            cx,
            id.name.as_str(),
            &new_expr.arguments,
            new_span,
        );
    }
    // §20.1.1 `new Object()` — empty-args shortcut to
    // `Op::NewObject`. One-arg / multi-arg forms fall through
    // to the general construct path so the runtime
    // `OrdinaryCreateFromConstructor` + `ToObject` (§20.1.1.1)
    // logic in `object_ctor_call` runs, including the
    // null/undefined → fresh-object coercion.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Object"
        && cx.lookup_binding("Object").is_none()
        && find_module_import_binding(cx, "Object").is_none()
        && new_expr.arguments.is_empty()
    {
        let dst = cx.alloc_scratch();
        cx.emit(Op::NewObject, [Operand::Register(dst)], new_span);
        return Ok(dst);
    }
    // §23.1.1.1 `new Array(...)` — typed
    // [`Op::ArrayConstruct`]. Single-numeric form reserves
    // a sparse array of that length; everything else
    // collects values like `Array.of`.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Array"
        && cx.lookup_binding("Array").is_none()
        && find_module_import_binding(cx, "Array").is_none()
    {
        let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::ArrayConstruct, operands, new_span);
        return Ok(dst);
    }
    // §22.1.1 `new String(value)` falls through to the
    // general constructor path so runtime bootstrap can
    // produce a String wrapper object with [[StringData]].
    // §21.1.1 `new Number(value)` no longer aliases here —
    // the `Number` global is now a real `ClassConstructor`
    // (see `bootstrap::install_number`) and the construct
    // form must produce a `NumberObject` wrapper with the
    // `[[NumberData]]` slot set, not a primitive Number.
    // Falls through to the general `NewExpression` path.
    // §20.3.1 `new Boolean(value)` falls through to the
    // general constructor path so runtime bootstrap can
    // produce a Boolean wrapper object with [[BooleanData]].
    // `new SharedArrayBuffer(length, options?)` routes
    // through the real `NativeFunction` installed by
    // `bootstrap_array_buffer` — `Op::SharedArrayBufferCall`
    // is no longer emitted from the compiler.
    // `new ArrayBuffer(length, options?)` routes through the
    // real `NativeFunction` installed by
    // `bootstrap_array_buffer` — `Op::ArrayBufferCall` is no
    // longer emitted from the compiler.
    // `new DataView(buffer, ...)` routes through the real
    // `NativeFunction` ctor installed by
    // `bootstrap_data_view` — `Op::DataViewCall` is no
    // longer emitted from the compiler.
    // `new <T>(...)` for each of the 11 concrete TypedArray
    // ctors routes through the real `NativeFunction`
    // installed by `bootstrap_typed_array` — the typed
    // `Op::TypedArrayCall` shortcut for the construct path
    // is no longer emitted. (Static-side `<T>.from(...)` /
    // `<T>.of(...)` shortcuts elsewhere stay in place until
    // the static methods are wired through the real ctor.)
    // §21.4.2 `new Date(...)` resolves against the real
    // `NativeFunction` constructor installed by the `Date`
    // bootstrap, so the dedicated `Op::DateCall` `Construct`
    // shortcut is no longer emitted.
    // §20.2.1.1 `new Function(arg0, …, body)` — every
    // argument coerces to a string at runtime; the leading
    // ones become parameter names and the last one is the
    // function body. Foundation lowers `Function(...)`
    // (without `new`) to the same shape per spec.
    // <https://tc39.es/ecma262/#sec-function-p1-p2-pn-body>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Function"
        && cx.lookup_binding("Function").is_none()
        && find_module_import_binding(cx, "Function").is_none()
    {
        let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::NewFunction, operands, new_span);
        return Ok(dst);
    }
    // `new Intl.<Class>(locale?, options?)` — dedicated
    // `Op::NewIntl` lowering. The callee is a static-member
    // expression `Intl.<Class>`; we pull the class name out
    // of the property and emit the constructor opcode.
    if let Expression::StaticMemberExpression(member) = callee
        && let Expression::Identifier(id) = &member.object
        && id.name.as_str() == "Intl"
        // NB: classes migrated to the spec-faithful `NativeCtx` option
        // ladder (firing getters in order) are deliberately ABSENT here so
        // they route through their real constructor instead of the
        // heap-only `Op::NewIntl` fast path: ListFormat, DurationFormat,
        // Locale.
        && matches!(member.property.name.as_str(), "NumberFormat" | "DisplayNames")
    {
        let class = member.property.name.as_str();
        let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
        let locale_reg = arg_regs.first().copied().unwrap_or_else(|| {
            let r = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(r)], new_span);
            r
        });
        let options_reg = arg_regs.get(1).copied().unwrap_or_else(|| {
            let r = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(r)], new_span);
            r
        });
        let dst = cx.alloc_scratch();
        let class_idx = cx.intern_string_constant(class);
        cx.emit(
            Op::NewIntl,
            vec![
                Operand::Register(dst),
                Operand::ConstIndex(class_idx),
                Operand::Register(locale_reg),
                Operand::Register(options_reg),
            ],
            new_span,
        );
        return Ok(dst);
    }
    // `new Map(iter?)` / `new Set(iter?)` /
    // `new WeakMap(iter?)` / `new WeakSet(iter?)` go through
    // the ordinary `Op::New` dispatch path now that the
    // bootstrap installs real `[[Construct]]` slots for each
    // global. The legacy dedicated `Op::NewCollection`
    // lowering bypassed `Map.prototype.set` / `Set.prototype.add`
    // and skipped the §7.4 iterator-protocol path, which is
    // observable through test262 `iterable-calls-set.js`.
    // `new WeakRef(target)` / `new FinalizationRegistry(cb)`
    // route through real `NativeFunction` ctors installed by
    // `bootstrap_weak_refs` — the dedicated `Op::NewWeakRef` /
    // `Op::NewFinalizationRegistry` lowering is no longer
    // needed.
    // §28.2.1 `new Proxy(target, handler)` resolves through the real
    // `NativeFunction` constructor installed by the `Proxy`
    // bootstrap, so the dedicated `Op::ProxyCall` `Construct`
    // shortcut is no longer emitted.
    // `new Promise(executor)` previously lowered to a
    // dedicated `Op::PromiseNew` that bypassed the real
    // `Promise` constructor. With the bootstrap installer
    // building a real callable + constructible
    // `NativeFunction`, the constructor flows through the
    // ordinary `Op::New` dispatch like every other
    // builtin (§27.2.3.1).
    // §13.3.5 NewExpression — positional arguments (spread is handled by the
    // early-return path above).
    let callee_reg = compile_expr(cx, callee, new_span)?;
    let arg_regs = compile_call_args(cx, &new_expr.arguments, new_span)?;
    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(callee_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::New, operands, new_span);
    Ok(dst)
}
