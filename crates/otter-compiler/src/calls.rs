//! Call-expression and spread-call lowering helpers.
//!
//! # Contents
//! - method calls
//! - spread calls
//! - Function method fast paths
//! - argument list compilation
//! - array spreading
//!
//! # Invariants
//! - Argument registers are emitted in source order.
//!
//! # See also
//! - `builtins_call` and `chain`

use crate::*;

/// Maximum argument count for dense `Op::Call` / `Op::CallWithThis`
/// / `Op::CallMethodValue` / `Op::BindFunction` lowering. The
/// bytecode wire encoder packs the per-instruction operand count
/// into one `u8`, so the variadic tail-payload of arguments tops out
/// at `u8::MAX` minus the fixed-position leading operands (`dst`,
/// optional callee/receiver, count-imm). 240 leaves enough headroom
/// across every variadic call op and matches the
/// `DENSE_NEW_ARRAY_MAX_ELEMENTS` threshold so the encoder layout is
/// uniform across families.
pub(crate) const MAX_DENSE_CALL_ARGS: usize = 240;

/// Reject argument lists that exceed [`MAX_DENSE_CALL_ARGS`] with a
/// catchable `CompileError` instead of panicking inside the wire
/// encoder. Generated code that fans out to thousands of positional
/// arguments should use spread (`f(...args)`), which goes through
/// `Op::CallSpread` and is unaffected by the cap.
pub(crate) fn check_call_arity(
    arg_count: usize,
    op_name: &'static str,
    span: (u32, u32),
) -> Result<(), CompileError> {
    if arg_count > MAX_DENSE_CALL_ARGS {
        return Err(CompileError::Unsupported {
            node: format!(
                "{op_name} with {arg_count} arguments exceeds the {MAX_DENSE_CALL_ARGS}-arg limit; \
                 use spread (`f(...args)`) for very wide call sites",
            ),
            span,
        });
    }
    Ok(())
}

/// Lower a call expression. Three forms are supported:
///
/// - `receiver.method(args...)` — emits [`Op::CallMethodValue`].
///   The runtime branches by receiver kind (string / array
///   intrinsics, plain object property dispatch, or
///   `Function.prototype.{call, apply, bind}` for callables).
/// - `callee.{call, apply, bind}(...)` with a syntactically obvious
///   call shape — lowered directly to [`Op::CallWithThis`] /
///   [`Op::BindFunction`] when the argument list can be flattened at
///   compile time. Dynamic `apply` argument lists stay on
///   [`Op::CallMethodValue`] so the VM performs the spec
///   `CreateListFromArrayLike` coercion.
/// - `callee(args...)` (free call) — emits [`Op::Call`]; the callee
///   receives `this = undefined`.
///
/// Computed-method access, `new`, and spread arguments are
/// deferred to later tasks.
pub(crate) fn compile_method_call(
    cx: &mut Compiler,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (call.span.start, call.span.end);
    let callee = unwrap_ts_expr(&call.callee);
    // `super(args...)` — direct super-constructor call. Only valid
    // inside a derived-class constructor; the upvalue lookup will
    // surface a clear diagnostic when used elsewhere.
    if let Expression::Super(_) = callee {
        return compile_super_call(cx, &call.arguments, span);
    }
    // `super.foo(args...)` — invoke a parent prototype method with
    // `this` bound to the current receiver.
    if let Expression::StaticMemberExpression(member) = callee
        && matches!(member.object, Expression::Super(_))
    {
        return compile_super_method_call(cx, member.property.name.as_str(), &call.arguments, span);
    }
    // `super[expr](args...)` — computed-key parent-method invocation.
    if let Expression::ComputedMemberExpression(member) = callee
        && matches!(member.object, Expression::Super(_))
    {
        return compile_super_computed_method_call(cx, &member.expression, &call.arguments, span);
    }
    // `import.meta.resolve(specifier)` — sync URL join against the
    // active module's URL. HTML spec returns a string; foundation
    // matches that shape via `Op::ImportMetaResolve`.
    // <https://html.spec.whatwg.org/multipage/webappapis.html#hostmetagetimportmetaproperties>
    if let Expression::StaticMemberExpression(member) = callee
        && let Expression::MetaProperty(meta) = &member.object
        && meta.meta.name.as_str() == "import"
        && meta.property.name.as_str() == "meta"
        && member.property.name.as_str() == "resolve"
    {
        if call.arguments.len() != 1 {
            return Err(CompileError::Unsupported {
                node: format!("import.meta.resolve/{}", call.arguments.len()),
                span,
            });
        }
        let arg = call.arguments[0]
            .as_expression()
            .ok_or(CompileError::Unsupported {
                node: "import.meta.resolve: spread argument".to_string(),
                span,
            })?;
        let spec_reg = compile_expr(cx, arg, span)?;
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::ImportMetaResolve,
            [Operand::Register(dst), Operand::Register(spec_reg)],
            span,
        );
        return Ok(dst);
    }
    // Bare `Error("msg")` / `TypeError("msg")` / etc. without
    // `new` is treated like the matching `new <Kind>("msg")` per
    // ES spec §20.5.1.1 — same lowering.
    if let Expression::Identifier(id) = callee
        && cx.lookup_binding(id.name.as_str()).is_none()
        && find_module_import_binding(cx, id.name.as_str()).is_none()
        && is_builtin_error_class_name(id.name.as_str())
        && builtin_error_construct_fast_path_applies(id.name.as_str(), &call.arguments)
    {
        return compile_builtin_error_construct(cx, id.name.as_str(), &call.arguments, span);
    }
    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, oxc_ast::ast::Argument::SpreadElement(_)));
    if has_spread {
        return compile_spread_call(cx, callee, &call.arguments, span);
    }
    if let Expression::StaticMemberExpression(member) = callee {
        // `ArrayBuffer.isView(arg)` routes through the real
        // `NativeFunction` installed by `bootstrap_array_buffer` —
        // `Op::ArrayBufferCall` for the static-method shape is no
        // longer emitted from the compiler.
        // §28.2.2 `Proxy.revocable(...)` flows through the real
        // `NativeFunction` installed by the `Proxy` bootstrap, so
        // the dedicated `Op::ProxyCall` shortcut is no longer
        // emitted.
        // §25.4 `Atomics.<method>(args)` — routed through the
        // namespace native function table installed by
        // `bootstrap::install_atomics`. The dedicated
        // `Op::AtomicsCall` shortcut was retired because it
        // bypassed the spec-required ToIndex coercion and
        // arraytype-before-value-coercion ordering. The opcode
        // handler in `crates/otter-vm/src/lib.rs` remains for
        // backwards-compatibility with older bytecode.
        // <https://tc39.es/ecma262/#sec-atomics-object>
        //
        // §28.1 `Reflect.<method>(args)` flows through the real
        // `NativeFunction` entries installed by the `Reflect`
        // namespace bootstrap, so the dedicated `Op::ReflectCall`
        // shortcut is no longer emitted. User shadowing of
        // `Reflect.<method>` is therefore observable.
        // Iterator-helpers proposal — `Iterator.from(...)` flows
        // through the real `NativeFunction` installed by the
        // `Iterator` bootstrap, so the dedicated `Op::IteratorCall`
        // shortcut is no longer emitted.
        // §23.2.2 TypedArray statics — `<T>.from(...)` / `<T>.of(...)`
        // flow through the real `NativeFunction` entries installed by
        // the per-TypedArray bootstrap, so the dedicated
        // `Op::TypedArrayCall` shortcut is no longer emitted.
        // §20.1.2 `Object.<method>(args)` — every static flows
        // through the real `NativeFunction` table installed by
        // `OBJECT_SPEC`. User shadowing of `Object.<method>` is
        // therefore observable per spec. No compile-time fast path:
        // the runtime IC caches the resolved callee.
        // §23.1.2 Array static surface. `Array.isArray` keeps a
        // dedicated [`Op::IsArray`] for the §7.2.2 fast path;
        // `Array.from` / `Array.of` lower to dedicated
        // [`Op::ArrayFrom`] / [`Op::ArrayOf`] opcodes.
        // <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Array"
        {
            let method = member.property.name.as_str();
            if method == "isArray" {
                let arg_regs = compile_call_args(cx, &call.arguments, span)?;
                let src = arg_regs.first().copied().unwrap_or_else(|| {
                    let undefined = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, [Operand::Register(undefined)], span);
                    undefined
                });
                let dst = cx.alloc_scratch();
                cx.emit(
                    Op::IsArray,
                    [Operand::Register(dst), Operand::Register(src)],
                    span,
                );
                return Ok(dst);
            }
            if matches!(method, "from" | "of") {
                let arg_regs = compile_call_args(cx, &call.arguments, span)?;
                let dst = cx.alloc_scratch();
                let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
                operands.push(Operand::Register(dst));
                operands.push(Operand::ConstIndex(arg_regs.len() as u32));
                operands.extend(arg_regs.iter().copied().map(Operand::Register));
                let opcode = if method == "from" {
                    Op::ArrayFrom
                } else {
                    Op::ArrayOf
                };
                cx.emit(opcode, operands, span);
                return Ok(dst);
            }
        }
        // `Math.<name>(args)` flows through the real `NativeFunction`
        // entries installed by the `Math` namespace bootstrap, so the
        // dedicated `Op::MathCall` shortcut is no longer emitted.
        // User shadowing of `Math.<method>` is therefore observable.
        // `JSON.<name>(args)` flows through the real `NativeFunction`
        // installed by the `JSON` namespace bootstrap, so the dedicated
        // `Op::JsonCall` shortcut is no longer emitted. User shadowing
        // of `JSON.parse` / `JSON.stringify` is therefore observable.
        // `Promise.<name>(args)` previously routed through
        // `Op::PromiseCall` for the typed dispatcher. With the
        // bootstrap installer placing real statics on the
        // `NativeFunction` constructor, the call flows through
        // ordinary method dispatch so user-installed shadows on
        // `Promise.<name>` are observable.
        // `Temporal.<Class>.<method>(args)` previously routed through
        // the dedicated `Op::TemporalCall` shortcut. The opcode is
        // gone; the call now falls through to ordinary dynamic
        // dispatch. The `Temporal` global bootstrap currently exposes
        // no concrete class statics, so most `Temporal.*` calls will
        // throw at runtime until that bootstrap is fleshed out.
        // `Symbol.<method>(args)` flows through the real `NativeFunction`
        // entries installed by the `Symbol` bootstrap, so the dedicated
        // `Op::SymbolCall` shortcut is no longer emitted. User shadowing
        // of `Symbol.for` / `Symbol.keyFor` is therefore observable.
        // §21.1.2 Number static surface — `Number.parseInt` /
        // `Number.parseFloat` / `Number.isNaN` / `Number.isFinite`
        // / `Number.isInteger` / `Number.isSafeInteger` flow through
        // the real `NativeFunction` entries installed by the
        // `Number` bootstrap, so the dedicated `Op::GlobalCall`
        // shortcut is no longer emitted.
    }
    // Bare `Symbol(desc)` resolves against the `NativeFunction`
    // installed by the `Symbol` bootstrap, so the dedicated
    // `Op::SymbolCall` `Construct` shortcut is no longer emitted.
    // User shadowing of `globalThis.Symbol` is therefore observable.
    // §20.3.1 `Boolean(value)` — coerces to boolean. The foundation
    // ships primitive-only Booleans (no wrapper object), so the
    // bare-call form is identical to `!!value`.
    // <https://tc39.es/ecma262/#sec-boolean-constructor>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Boolean"
        && cx.lookup_binding("Boolean").is_none()
        && find_module_import_binding(cx, "Boolean").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        match arg_regs.first().copied() {
            Some(src) => {
                cx.emit(
                    Op::ToBoolean,
                    [Operand::Register(dst), Operand::Register(src)],
                    span,
                );
            }
            None => {
                cx.emit(Op::LoadFalse, [Operand::Register(dst)], span);
            }
        }
        return Ok(dst);
    }
    // §22.1.1 / §22.1.2 — bare-call `String(value)` and the
    // `String.<method>(args)` statics route through the
    // `Value::NativeFunction` table installed at bootstrap. The
    // dedicated `Op::StringCall` shortcut was retired because it
    // bypassed §7.1.17 ToString's ToPrimitive step (user classes
    // with an overridden `toString` / `Symbol.toPrimitive` did
    // not fire), and because spec-shaped dispatch through
    // ordinary property lookup is the simpler invariant to keep.
    // The opcode handler in `crates/otter-vm/src/lib.rs` remains
    // for backwards-compatibility with older bytecode.
    // <https://tc39.es/ecma262/#sec-string-constructor>
    // §21.1.1 `Number(value)` fast path — folds to `Op::LoadInt32 0`
    // for the bare zero-arg form, or to `Op::ToNumber` for a single
    // primitive arg. The BigInt arm of §21.1.1.1 step 5 takes a
    // different shape than generic §7.1.4 ToNumber (which throws on
    // BigInt — see `language/expressions/unary-plus/bigint-throws.js`).
    // BigInt arity can't be observed at compile time, so callers with
    // any argument fall through to the ordinary call dispatch which
    // routes to `number_ctor_call` and the spec-correct BigInt arm.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Number"
        && cx.lookup_binding("Number").is_none()
        && find_module_import_binding(cx, "Number").is_none()
        && call.arguments.is_empty()
    {
        let dst = cx.alloc_scratch();
        cx.emit(
            Op::LoadInt32,
            [Operand::Register(dst), Operand::Imm32(0)],
            span,
        );
        return Ok(dst);
    }
    // §20.3.1 `Boolean(value)` — primitive ToBoolean.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Boolean"
        && cx.lookup_binding("Boolean").is_none()
        && find_module_import_binding(cx, "Boolean").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        match arg_regs.first().copied() {
            Some(src) => cx.emit(
                Op::ToBoolean,
                [Operand::Register(dst), Operand::Register(src)],
                span,
            ),
            None => cx.emit(Op::LoadFalse, [Operand::Register(dst)], span),
        }
        return Ok(dst);
    }
    // §23.1.1.1 `Array(...)` — bare-call form has the same spec
    // body as `new Array(...)`. Both lower to [`Op::ArrayConstruct`]
    // so the single-numeric-length form produces a sparse array.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Array"
        && cx.lookup_binding("Array").is_none()
        && find_module_import_binding(cx, "Array").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::ArrayConstruct, operands, span);
        return Ok(dst);
    }
    // §20.1.1 `Object()` — empty-args shortcut to `Op::NewObject`.
    // One-arg form falls through to the general call path so the
    // runtime `object_ctor_call` (§20.1.1.1) handles null/undefined
    // → fresh-object coercion and primitive → wrapper coercion.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Object"
        && cx.lookup_binding("Object").is_none()
        && find_module_import_binding(cx, "Object").is_none()
        && call.arguments.is_empty()
    {
        let dst = cx.alloc_scratch();
        cx.emit(Op::NewObject, [Operand::Register(dst)], span);
        return Ok(dst);
    }
    // §21.4.3 `Date.<method>(args)` — `Date.now` / `Date.parse` /
    // `Date.UTC` flow through the real `NativeFunction` entries
    // installed by the `Date` bootstrap. The dedicated `Op::DateCall`
    // shortcut for the static surface is no longer emitted, so user
    // shadows of `Date.<method>` are observable per ECMA-262.
    // `BigInt(value)` and `BigInt.asIntN/asUintN(args)` route
    // through the real `NativeFunction` installed by
    // `bootstrap_bigint` — the dedicated `Op::BigIntCall` shortcut
    // is no longer emitted.
    // §20.2.1.1 — bare `Function(arg0, …, body)` is the same as
    // `new Function(...)` per spec; lower both shapes through one
    // path.
    // <https://tc39.es/ecma262/#sec-function-p1-p2-pn-body>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "Function"
        && cx.lookup_binding("Function").is_none()
        && find_module_import_binding(cx, "Function").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::NewFunction, operands, span);
        return Ok(dst);
    }
    // §19.4.1 `eval(source)` — bare-identifier interception.
    // Foundation ships indirect-eval semantics (fresh global
    // scope) which keeps the implementation tractable while
    // covering the common use case of running source-string
    // payloads at runtime.
    // <https://tc39.es/ecma262/#sec-eval-x>
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "eval"
        && cx.lookup_binding("eval").is_none()
        && find_module_import_binding(cx, "eval").is_none()
    {
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        if arg_regs.is_empty() {
            let dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
            return Ok(dst);
        }
        let src_reg = arg_regs[0];
        let dst = cx.alloc_scratch();
        // §19.2.1.3 EvalDeclarationInstantiation — a sloppy direct
        // eval run from a parameter initializer whose body
        // var-declares `arguments` throws SyntaxError when the
        // calling function binds the name (the binding exists but is
        // still uninitialized during parameter instantiation). After
        // the parameters are bound the same eval body is legal, so
        // only the parameter-default window arms the flag. Arrows
        // have lexical [[ThisMode]] — the restriction never applies
        // to an arrow variable environment.
        let forbid_var_arguments = cx.in_param_init && cx.binds_arguments;
        // §19.2.1.1 step 5 — `new.target` in the eval body is legal
        // only when the call site sits inside *non-arrow* function
        // code (arrows are transparent: the signal comes from the
        // enclosing function), or inside a class field initializer.
        let new_target_allowed = cx.eval_new_target_allowed
            || cx.in_field_initializer
            || cx.stack.iter().skip(1).any(|frame| !frame.is_arrow);
        // Flag bits: 0 — forbid var-`arguments` (§19.2.1.3); 1 — the
        // call site sits in a parameter initializer, where the
        // caller's *body* lexical bindings are not yet in scope;
        // 2 — `new.target` is legal in the eval body; 3 — the body
        // observes `new.target` as undefined (field initializer).
        // 4 — the call site's context carries a [[HomeObject]]
        // (method body or field initializer), so `super.x` is legal
        // in the eval body (§19.2.1.1 step 5).
        let super_allowed = cx.in_field_initializer
            || cx.lookup_binding(crate::class::SUPER_HOME_NAME).is_some()
            || cx.resolve_capture(crate::class::SUPER_HOME_NAME).is_some()
            || cx
                .resolve_capture(crate::class::SUPER_STATIC_HOME_NAME)
                .is_some();
        let flags = i32::from(forbid_var_arguments)
            | (i32::from(cx.in_param_init) << 1)
            | (i32::from(new_target_allowed) << 2)
            | (i32::from(cx.in_field_initializer) << 3)
            | (i32::from(super_allowed) << 4);
        cx.emit(
            Op::Eval,
            [
                Operand::Register(dst),
                Operand::Register(src_reg),
                Operand::Imm32(flags),
            ],
            span,
        );
        return Ok(dst);
    }
    // §19.2 global function bare-identifier calls like
    // `parseInt(...)` / `isNaN(x)` / `encodeURIComponent(s)` flow
    // through the real `NativeFunction` entries installed on
    // `globalThis`, so the dedicated `Op::GlobalCall` shortcut is
    // no longer emitted.
    // Bare-identifier interceptions — `queueMicrotask(fn, ...args)`
    // is the only one today. Lives at the call-site layer (not
    // inside the StaticMember branch) because the syntax is a
    // direct call, not a method call.
    if let Expression::Identifier(id) = callee
        && id.name.as_str() == "queueMicrotask"
    {
        // Compile arguments first so any side effects in the args
        // run before the enqueue, matching JS evaluation order.
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        if arg_regs.is_empty() {
            return Err(CompileError::Unsupported {
                node: "queueMicrotask requires a callback argument".to_string(),
                span,
            });
        }
        let mut iter = arg_regs.into_iter();
        let callee_reg = iter.next().expect("checked non-empty");
        let trailing: Vec<u16> = iter.collect();
        let mut operands: Vec<Operand> = Vec::with_capacity(2 + trailing.len());
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::ConstIndex(trailing.len() as u32));
        operands.extend(trailing.into_iter().map(Operand::Register));
        cx.emit(Op::QueueMicrotask, operands, span);
        // queueMicrotask returns `undefined` synchronously.
        let dst = cx.alloc_scratch();
        cx.emit(Op::LoadUndefined, [Operand::Register(dst)], span);
        return Ok(dst);
    }
    if let Expression::StaticMemberExpression(member) = callee {
        let method_name = member.property.name.as_str();
        if let Some(dst) =
            try_compile_function_method(cx, &member.object, method_name, &call.arguments, span)?
        {
            return Ok(dst);
        }
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        let name_idx = cx.intern_string_constant(method_name);
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallMethodValue", span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(name_idx));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallMethodValue, operands, span);
        return Ok(dst);
    }
    if let Expression::PrivateFieldExpression(member) = callee {
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        let key_reg = crate::class::load_private_key(cx, member.field.name.as_str(), span)?;
        let callee_reg = cx.alloc_scratch();
        cx.emit(
            Op::PrivateGet,
            [
                Operand::Register(callee_reg),
                Operand::Register(receiver_reg),
                Operand::Register(key_reg),
            ],
            span,
        );
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallMethodValue", span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
        return Ok(dst);
    }
    // `obj[expr](args...)` — computed-member call. Lower as
    // `LoadElement` + `CallWithThis` so the callee receives the
    // receiver as its `this` value, matching ECMA-262 §13.3.6.1
    // EvaluateCall step 5.b.
    if let Expression::ComputedMemberExpression(member) = callee {
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        let idx_reg = compile_expr(cx, &member.expression, span)?;
        let callee_reg = cx.alloc_scratch();
        cx.emit(
            Op::LoadElement,
            vec![
                Operand::Register(callee_reg),
                Operand::Register(receiver_reg),
                Operand::Register(idx_reg),
            ],
            span,
        );
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        check_call_arity(arg_regs.len(), "Op::CallWithThis", span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(callee_reg));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallWithThis, operands, span);
        return Ok(dst);
    }
    // Free call: `callee(args...)`.
    let callee_reg = compile_expr(cx, callee, span)?;
    let arg_regs = compile_call_args(cx, &call.arguments, span)?;
    check_call_arity(arg_regs.len(), "Op::Call", span)?;
    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(callee_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::Call, operands, span);
    Ok(dst)
}

/// Lower a call expression whose argument list contains at least
/// one `...spread` element to [`Op::CallSpread`]. Two callee
/// shapes are handled:
///
/// - `obj.method(...args)` — receiver is evaluated once, the spread
///   args become an array, dispatched with `this = obj`.
/// - `callee(...args)` — free call, dispatched with
///   `this = undefined`.
///
/// Mixed spread / non-spread arguments are folded into the same
/// args array so `f(a, ...arr, b)` calls `f(a, ...arr items..., b)`.
pub(crate) fn compile_spread_call(
    cx: &mut Compiler,
    callee: &Expression<'_>,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let (callee_reg, this_reg) = match callee {
        Expression::StaticMemberExpression(member) => {
            let recv = compile_expr(cx, &member.object, span)?;
            let name_idx = cx.intern_string_constant(member.property.name.as_str());
            let method_dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(method_dst),
                    Operand::Register(recv),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            (method_dst, recv)
        }
        other => {
            let r = compile_expr(cx, other, span)?;
            let this_dst = cx.alloc_scratch();
            cx.emit(Op::LoadUndefined, [Operand::Register(this_dst)], span);
            (r, this_dst)
        }
    };
    let args_reg = compile_spread_call_args(cx, arguments, span)?;
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::CallSpread,
        vec![
            Operand::Register(dst),
            Operand::Register(callee_reg),
            Operand::Register(this_reg),
            Operand::Register(args_reg),
        ],
        span,
    );
    Ok(dst)
}

/// Lower the syntactic shapes `<expr>.call(...)`, `<expr>.apply(...)`,
/// and `<expr>.bind(...)` directly to dedicated opcodes. Returns
/// `None` when `method_name` is not one of the recognised triple,
/// so the caller can fall through to the universal
/// [`Op::CallMethodValue`] path.
///
/// The shape detection is **syntactic**: the receiver expression is
/// evaluated only once, so `getFn().call(t, 1)` invokes `getFn()`
/// exactly once. `apply` uses the fixed-arity [`Op::CallWithThis`]
/// path for array literals and falls back to [`Op::CallSpread`] for
/// dynamic argument arrays so the runtime performs the observable
/// argument-list check.
pub(crate) fn try_compile_function_method(
    cx: &mut Compiler,
    receiver: &Expression<'_>,
    method_name: &str,
    arguments: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<Option<u16>, CompileError> {
    match method_name {
        "call" => {
            let callee_reg = compile_expr(cx, receiver, span)?;
            let arg_regs = compile_call_args(cx, arguments, span)?;
            let mut iter = arg_regs.into_iter();
            let this_reg = match iter.next() {
                Some(r) => r,
                None => {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
                    r
                }
            };
            let forwarded: Vec<u16> = iter.collect();
            check_call_arity(forwarded.len(), "Op::CallWithThis", span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + forwarded.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::Register(this_reg));
            operands.push(Operand::ConstIndex(forwarded.len() as u32));
            operands.extend(forwarded.into_iter().map(Operand::Register));
            cx.emit(Op::CallWithThis, operands, span);
            Ok(Some(dst))
        }
        "bind" => {
            let callee_reg = compile_expr(cx, receiver, span)?;
            let arg_regs = compile_call_args(cx, arguments, span)?;
            let mut iter = arg_regs.into_iter();
            let this_reg = match iter.next() {
                Some(r) => r,
                None => {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
                    r
                }
            };
            let bound: Vec<u16> = iter.collect();
            check_call_arity(bound.len(), "Op::BindFunction", span)?;
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + bound.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::Register(this_reg));
            operands.push(Operand::ConstIndex(bound.len() as u32));
            operands.extend(bound.into_iter().map(Operand::Register));
            cx.emit(Op::BindFunction, operands, span);
            Ok(Some(dst))
        }
        "apply" => {
            // `apply(thisArg, argsArray)` — accepts exactly two
            // observable arguments per §20.2.3.1. Extra trailing
            // arguments are evaluated but ignored by the spec, so
            // they must remain side-effect-observable. Bail to the
            // universal `CallMethodValue` dispatch path so the
            // receiver isn't compiled twice and every arg gets
            // evaluated in source order.
            if arguments.len() > 2 {
                return Ok(None);
            }
            // Spread at the top-level argument list (e.g.
            // `fn.apply(...args)`) is dispatched via `compile_spread_call`
            // before reaching here, so we never see `SpreadElement`
            // as a direct argument.
            let callee_reg = compile_expr(cx, receiver, span)?;
            let mut args_iter = arguments.iter();
            let this_reg = match args_iter.next() {
                Some(other) => compile_expr(cx, other.to_expression(), span)?,
                None => {
                    let r = cx.alloc_scratch();
                    cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
                    r
                }
            };
            let mut forwarded: Vec<u16> = Vec::new();
            let mut dynamic_args: Option<u16> = None;
            if let Some(other) = args_iter.next() {
                let expr = unwrap_ts_expr(other.to_expression());
                match expr {
                    Expression::ArrayExpression(arr)
                        if !arr.elements.iter().any(|el| {
                            matches!(el, oxc_ast::ast::ArrayExpressionElement::SpreadElement(_))
                        }) =>
                    {
                        for el in &arr.elements {
                            match el {
                                oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => {
                                    unreachable!("spread excluded above")
                                }
                                oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                                    let r = cx.alloc_scratch();
                                    cx.emit(Op::LoadUndefined, [Operand::Register(r)], span);
                                    forwarded.push(r);
                                }
                                el_expr => {
                                    forwarded.push(compile_expr(
                                        cx,
                                        el_expr.to_expression(),
                                        span,
                                    )?);
                                }
                            }
                        }
                    }
                    Expression::NullLiteral(_) => {}
                    Expression::Identifier(id) if id.name.as_str() == "undefined" => {}
                    _ => {
                        // Array literals with `...spread` and any
                        // other expression shape go through the
                        // dynamic path so the runtime performs the
                        // spec-required iterable coercion.
                        dynamic_args = Some(compile_expr(cx, expr, span)?);
                    }
                }
            }
            let dst = cx.alloc_scratch();
            if let Some(args_reg) = dynamic_args {
                let name_idx = cx.intern_string_constant("apply");
                cx.emit(
                    Op::CallMethodValue,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(callee_reg),
                        Operand::ConstIndex(name_idx),
                        Operand::ConstIndex(2),
                        Operand::Register(this_reg),
                        Operand::Register(args_reg),
                    ],
                    span,
                );
                return Ok(Some(dst));
            }
            let mut operands: Vec<Operand> = Vec::with_capacity(4 + forwarded.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::Register(callee_reg));
            operands.push(Operand::Register(this_reg));
            operands.push(Operand::ConstIndex(forwarded.len() as u32));
            operands.extend(forwarded.into_iter().map(Operand::Register));
            check_call_arity(operands.len().saturating_sub(4), "Op::CallWithThis", span)?;
            cx.emit(Op::CallWithThis, operands, span);
            Ok(Some(dst))
        }
        _ => Ok(None),
    }
}

pub(crate) fn compile_call_args(
    cx: &mut Compiler,
    args: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<Vec<u16>, CompileError> {
    let mut regs: Vec<u16> = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            oxc_ast::ast::Argument::SpreadElement(s) => {
                return Err(CompileError::Unsupported {
                    node: "Argument::SpreadElement".to_string(),
                    span: (s.span.start, s.span.end),
                });
            }
            other => {
                let expr = other.to_expression();
                regs.push(compile_expr(cx, expr, span)?);
            }
        }
    }
    Ok(regs)
}

/// Emit the bytecode that builds a fresh `Array` register holding
/// the call arguments fanned out from spreads. Returns the
/// register that holds the resulting array. Used by the spread-in-
/// call path; pure regular argument lists keep the dedicated
/// fast path in [`compile_call_args`] / [`Op::Call`].
pub(crate) fn compile_spread_call_args(
    cx: &mut Compiler,
    args: &oxc_allocator::Vec<'_, oxc_ast::ast::Argument<'_>>,
    span: (u32, u32),
) -> Result<u16, CompileError> {
    let dst = cx.alloc_scratch();
    cx.emit(
        Op::NewArray,
        [Operand::Register(dst), Operand::ConstIndex(0)],
        span,
    );
    for arg in args {
        match arg {
            oxc_ast::ast::Argument::SpreadElement(s) => {
                let inner_span = (s.span.start, s.span.end);
                emit_spread_into_array(cx, dst, &s.argument, inner_span)?;
            }
            other => {
                let r = compile_expr(cx, other.to_expression(), span)?;
                cx.emit(
                    Op::ArrayPush,
                    [Operand::Register(dst), Operand::Register(r)],
                    span,
                );
            }
        }
    }
    Ok(dst)
}

/// Append every element of `iterable` (already materialised as an
/// expression) into the array in `dst_reg`. Lowered as a tight
/// `IteratorNext` loop over a fresh iterator. Shared between the
/// array-literal spread path and the call-argument spread path.
pub(crate) fn emit_spread_into_array(
    cx: &mut Compiler,
    dst_reg: u16,
    iterable: &Expression<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let iterable_reg = compile_expr(cx, iterable, span)?;
    let iter_reg = cx.alloc_scratch();
    cx.emit(
        Op::GetIterator,
        [Operand::Register(iter_reg), Operand::Register(iterable_reg)],
        span,
    );
    let value_reg = cx.alloc_scratch();
    let done_reg = cx.alloc_scratch();
    let loop_top = cx.next_pc;
    cx.emit(
        Op::IteratorNext,
        vec![
            Operand::Register(value_reg),
            Operand::Register(done_reg),
            Operand::Register(iter_reg),
        ],
        span,
    );
    let exit = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(done_reg), span);
    cx.emit(
        Op::ArrayPush,
        [Operand::Register(dst_reg), Operand::Register(value_reg)],
        span,
    );
    let back = cx.emit_branch_placeholder(Op::Jump, None, span);
    cx.patch_branch(back, loop_top);
    cx.patch_branch_to_here(exit);
    Ok(())
}
