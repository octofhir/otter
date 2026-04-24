//! Call-expression lowering: `f(...)`, `o.m(...)`, `o[k](...)`,
//! `#m(...)`, `super(...)`, and all the variants with spread.
//!
//! Extracted from `source_compiler/mod.rs` in the file-size cleanup
//! follow-up after C4. The public entry points are
//! `lower_call_expression` plus the private-name / super-property
//! guards (`enforce_private_name_declared`,
//! `enforce_super_property_binding`) and the read-side helpers
//! (`lower_private_field_read`, `lower_private_in_expression`) — the
//! rest of the module (direct / super / private-method / computed /
//! spread / arg-staging) is internal.

use super::*;

/// Lowers a `CallExpression`. Four callee shapes are accepted:
///
/// - Identifier naming a top-level `FunctionDeclaration` — emits
///   `CallDirect func_idx, argv` for the tightest invocation path
///   (known callee, direct index, tier-up-friendly).
/// - `o.method(args)` (StaticMemberExpression callee) — emits
///   `CallProperty r_callee, r_receiver, argv`; `this` is bound to
///   the member's base per §13.3.6.
/// - `o[k](args)` (ComputedMemberExpression callee) — same opcode,
///   key resolved via `LdaKeyedProperty`.
/// - Any other expression that evaluates to a callable —
///   `(function(){})()`, `factory()()`, `(cond ? f : g)()` — emits
///   `CallUndefinedReceiver` / `CallSpread` with `this = undefined`.
///
/// Direct-call shape:
///
/// ```text
///   <lower arg 0>; Star r_arg0
///   <lower arg 1>; Star r_arg1
///   …
///   CallDirect func_idx, RegList { base: r_arg0, count: argc }
/// ```
///
/// Method-call shape:
///
/// ```text
///   <lower receiver>; Star r_receiver
///   <lower callee from r_receiver>; Star r_callee
///   <lower arg 0>; Star r_arg0
///   …
///   CallProperty r_callee, r_receiver, RegList { base: r_arg0, count: argc }
/// ```
///
/// Temps are acquired from the function-level pool
/// ([`LoweringContext::acquire_temps`]) so nested calls get
/// non-overlapping windows; release is LIFO.
pub(super) fn lower_call_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;

    // §13.3.9 `f?.()` — the callee value is evaluated first, then
    // nullish-checked against the active chain's short-circuit
    // label. This path handles the identifier-callee and
    // member-callee cases by routing through a dynamic-dispatch
    // helper.
    if call.optional {
        let Some(short_circuit) = ctx.optional_chain_short_circuit() else {
            return Err(SourceLoweringError::unsupported(
                "optional_call_expression",
                call.span,
            ));
        };
        return lower_optional_call(builder, ctx, call, short_circuit);
    }

    // Callee classification — strip a single layer of parens so
    // `(f)()` still works, then match on the inner shape. Member
    // callees go through the method-call path so `this` binds
    // correctly; everything else goes through the direct-call
    // path.
    let inner_callee = match &call.callee {
        Expression::ParenthesizedExpression(paren) => &paren.expression,
        other => other,
    };

    // M23: any `...expr` argument forces the CallSpread path.
    // Direct calls use `undefined` as the receiver; method calls
    // preserve their evaluated receiver.
    let has_spread = call
        .arguments
        .iter()
        .any(|arg| matches!(arg, Argument::SpreadElement(_)));

    match inner_callee {
        Expression::Identifier(ident) => {
            if has_spread {
                return lower_direct_call_with_spread(builder, ctx, call, ident);
            }
            lower_direct_call(builder, ctx, call, ident)
        }
        Expression::StaticMemberExpression(member) => {
            lower_static_method_call(builder, ctx, call, member, has_spread)
        }
        Expression::ComputedMemberExpression(member) => {
            lower_computed_method_call(builder, ctx, call, member, has_spread)
        }
        // M29.5: `obj.#m(args)` — private method invocation.
        // Callee comes from `GetPrivateField` with `obj` as the
        // receiver; the call itself still passes `obj` as `this`.
        Expression::PrivateFieldExpression(member) => {
            lower_private_method_call(builder, ctx, call, member, has_spread)
        }
        // M28: `super(args)` — §13.3.7.1 SuperCall. Allowed only
        // inside a derived-class constructor (enforced via the
        // `ClassSuperBinding` on this `LoweringContext`). Args land
        // in a contiguous temp window, then `CallSuper` /
        // `CallSuperSpread` does the construct + receiver
        // initialization.
        Expression::Super(super_tok) => {
            lower_super_call(builder, ctx, call, super_tok.span, has_spread)
        }
        other => lower_expression_direct_call(builder, ctx, call, other, has_spread),
    }
}

/// Lowers `super(args)` / `super(...args)` inside a derived-class
/// constructor. Emits `CallSuper` for fixed-arity calls and
/// `CallSuperSpread` when any argument is spread.
///
/// Rejection surface:
/// - `super_outside_class`: active function has no
///   `ClassSuperBinding` (plain function / top-level code).
/// - `super_call_in_non_derived_class`: `ClassSuperBinding` is set
///   but `allow_super_call` is false (base-class constructor or
///   method body).
fn lower_super_call<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    super_span: Span,
    has_spread: bool,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;
    let binding = ctx
        .class_super_binding
        .ok_or_else(|| SourceLoweringError::unsupported("super_outside_class", super_span))?;
    if !binding.allow_super_call {
        return Err(SourceLoweringError::unsupported(
            "super_call_in_non_derived_class",
            super_span,
        ));
    }

    if !has_spread {
        let argc = RegisterIndex::try_from(call.arguments.len()).map_err(|_| {
            SourceLoweringError::Internal("super argument count exceeds u16".into())
        })?;
        let args_base = if argc == 0 {
            0
        } else {
            ctx.acquire_temps(argc)?
        };
        let lower = (|| -> Result<(), SourceLoweringError> {
            for (offset, arg) in call.arguments.iter().enumerate() {
                let expr = match arg {
                    Argument::SpreadElement(_) => unreachable!("rejected above"),
                    other => other.to_expression(),
                };
                lower_return_expression(builder, ctx, expr)?;
                let slot = args_base
                    .checked_add(RegisterIndex::try_from(offset).map_err(|_| {
                        SourceLoweringError::Internal("super arg offset overflow".into())
                    })?)
                    .ok_or_else(|| {
                        SourceLoweringError::Internal("super arg slot overflow".into())
                    })?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode Star (super arg): {err:?}"))
                    })?;
            }
            let call_pc = builder
                .emit(
                    Opcode::CallSuper,
                    &[Operand::RegList {
                        base: u32::from(args_base),
                        count: u32::from(argc),
                    }],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode CallSuper: {err:?}"))
                })?;
            ctx.attach_call_feedback(builder, call_pc);
            Ok(())
        })();
        if argc > 0 {
            ctx.release_temps(argc);
        }
        return lower;
    }

    // Spread path — build an Array of args, then CallSuperSpread.
    let args_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode CreateArray (super spread args): {err:?}"
            ))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(args_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (super spread args): {err:?}"))
            })?;
        for arg in call.arguments.iter() {
            match arg {
                Argument::SpreadElement(spread) => {
                    lower_return_expression(builder, ctx, &spread.argument)?;
                    builder
                        .emit(
                            Opcode::SpreadIntoArray,
                            &[Operand::Reg(u32::from(args_temp))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode SpreadIntoArray (super arg): {err:?}"
                            ))
                        })?;
                }
                other => {
                    lower_return_expression(builder, ctx, other.to_expression())?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(args_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode ArrayPush (super arg): {err:?}"
                            ))
                        })?;
                }
            }
        }
        let call_pc = builder
            .emit(
                Opcode::CallSuperSpread,
                &[Operand::RegList {
                    base: u32::from(args_temp),
                    count: 1,
                }],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CallSuperSpread: {err:?}"))
            })?;
        ctx.attach_call_feedback(builder, call_pc);
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Direct-call path: `f(args)` where `f` names a known top-level
/// function in the same module. Emits `CallDirect` so the
/// interpreter can resolve the callee by function index without a
/// property lookup or an object handle.
fn lower_direct_call<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    callee_ident: &IdentifierReference<'a>,
) -> Result<(), SourceLoweringError> {
    let name = callee_ident.name.as_str();
    // Preferred: the name resolves to a top-level
    // `FunctionDeclaration`. Emit `CallDirect <idx>, args`.
    if let Some(func_idx) = ctx.resolve_function(name) {
        let argc = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
        let base = ctx.acquire_temps(argc)?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            lower_call_arguments_into_temps(builder, ctx, call, base)?;
            let call_pc = builder
                .emit(
                    Opcode::CallDirect,
                    &[
                        Operand::Idx(func_idx.0),
                        Operand::RegList {
                            base: u32::from(base),
                            count: u32::from(argc),
                        },
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode CallDirect: {err:?}"))
                })?;
            ctx.attach_call_feedback(builder, call_pc);
            Ok(())
        })();
        ctx.release_temps(argc);
        return lower;
    }
    // Fallback: the name binds a local / param holding a
    // callable value (a closure from a FunctionExpression, for
    // instance). Load the value into a reg, then dispatch via
    // `CallUndefinedReceiver` — same path a plain-function
    // reference takes.
    if let Some(binding) = ctx.resolve_identifier(name) {
        // Acquire a callee temp + argc arg temps. The callee temp
        // holds the callable value (either loaded from a reg via
        // `Ldar` or from an upvalue via `LdaUpvalue`).
        let argc = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
        let callee_temp = ctx.acquire_temps(1)?;
        let args_base = ctx
            .acquire_temps(argc)
            .inspect_err(|_| ctx.release_temps(1))?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            emit_load_binding_value(builder, binding, callee_ident.span, "callable binding")?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (callable temp): {err:?}"))
                })?;
            lower_call_arguments_into_temps(builder, ctx, call, args_base)?;
            let call_pc = builder
                .emit(
                    Opcode::CallUndefinedReceiver,
                    &[
                        Operand::Reg(u32::from(callee_temp)),
                        Operand::RegList {
                            base: u32::from(args_base),
                            count: u32::from(argc),
                        },
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CallUndefinedReceiver (local callable): {err:?}"
                    ))
                })?;
            ctx.attach_call_feedback(builder, call_pc);
            Ok(())
        })();
        ctx.release_temps(argc);
        ctx.release_temps(1);
        return lower;
    }
    // Last resort: script-mode global lookup. If the binding is
    // absent at runtime, `LdaGlobal` throws ReferenceError before
    // the call dispatch, matching ordinary ECMAScript semantics.
    let argc = RegisterIndex::try_from(call.arguments.len())
        .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = ctx
        .acquire_temps(argc)
        .inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        let idx = ctx.intern_property_name(name)?;
        builder
            .emit(Opcode::LdaGlobal, &[Operand::Idx(idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaGlobal (global callable): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (global callable temp): {err:?}"
                ))
            })?;
        lower_call_arguments_into_temps(builder, ctx, call, args_base)?;
        let call_pc = builder
            .emit(
                Opcode::CallUndefinedReceiver,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: u32::from(argc),
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode CallUndefinedReceiver (global callable): {err:?}"
                ))
            })?;
        ctx.attach_call_feedback(builder, call_pc);
        Ok(())
    })();
    ctx.release_temps(argc);
    ctx.release_temps(1);
    lower
}

/// Spread-argument direct call: `f(...args)` / `f(a, ...rest)`.
/// Loads the callee value into a temp (via the same binding /
/// global / closure resolution the non-spread path uses), sets
/// the receiver to `undefined`, builds a single Array from the
/// spread + plain arguments, and dispatches via `CallSpread`.
fn lower_direct_call_with_spread<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &'a oxc_ast::ast::CallExpression<'a>,
    callee_ident: &'a IdentifierReference<'a>,
) -> Result<(), SourceLoweringError> {
    let name = callee_ident.name.as_str();
    let callee_temp = ctx.acquire_temps(1)?;
    let receiver_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let args_base = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        // 1) Resolve the callee identifier into a value and spill
        //    it into `callee_temp`. The resolution ladder mirrors
        //    the non-spread `lower_direct_call`: local / param,
        //    upvalue, top-level function (via `CreateClosure` of
        //    the `FunctionIndex`), then the global fallback.
        if let Some(binding) = ctx.resolve_identifier(name) {
            emit_load_binding_value(builder, binding, callee_ident.span, "spread callee")?;
        } else if let Some(func_idx) = ctx.resolve_function(name) {
            // Top-level function declaration — materialise the
            // closure inline via `CreateClosure <func_idx>, 0`
            // so we don't depend on the runtime having already
            // installed the global (matters for test harnesses
            // that invoke declared functions directly without
            // running the synth top-level first).
            let pc = builder
                .emit(
                    Opcode::CreateClosure,
                    &[Operand::Idx(func_idx.0), Operand::Imm(0)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateClosure (spread callee): {err:?}"
                    ))
                })?;
            ctx.record_closure_template(
                pc,
                crate::closure::ClosureTemplate::new(func_idx, Vec::new()),
            );
        } else {
            let idx = ctx.intern_property_name(name)?;
            builder
                .emit(Opcode::LdaGlobal, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaGlobal (spread callee): {err:?}"
                    ))
                })?;
        }
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (spread callee): {err:?}"))
            })?;

        // 2) Receiver = undefined. Direct calls have no implicit
        //    receiver; the runtime passes `undefined` to the
        //    callee's `this`.
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined (spread recv): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (spread recv): {err:?}"))
            })?;

        // 3) Build the argument array: start with an empty
        //    array, push each plain arg, spread each `...expr`
        //    arg via the existing SpreadIntoArray opcode.
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode CreateArray (spread direct-call): {err:?}"
            ))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(args_base))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (spread direct-call args): {err:?}"
                ))
            })?;
        for arg in call.arguments.iter() {
            match arg {
                oxc_ast::ast::Argument::SpreadElement(spread) => {
                    lower_return_expression(builder, ctx, &spread.argument)?;
                    builder
                        .emit(
                            Opcode::SpreadIntoArray,
                            &[Operand::Reg(u32::from(args_base))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode SpreadIntoArray (spread direct-call): {err:?}"
                            ))
                        })?;
                }
                other => {
                    lower_return_expression(builder, ctx, other.to_expression())?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(args_base))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode ArrayPush (spread direct-call): {err:?}"
                            ))
                        })?;
                }
            }
        }

        // 4) Dispatch through CallSpread — same opcode method
        //    calls use when any arg is a spread.
        let call_pc = builder
            .emit(
                Opcode::CallSpread,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(receiver_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: 1,
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CallSpread (direct call): {err:?}"))
            })?;
        ctx.attach_call_feedback(builder, call_pc);
        Ok(())
    })();
    ctx.release_temps(3);
    lower
}

/// Method-call path for `o.method(args)`. Receiver, callee, and
/// each argument each go into a dedicated temp so `CallProperty`
/// sees three register operands plus a contiguous arg window.
/// Method name is interned into the function's
/// `PropertyNameTable`, matching the M17 `LdaNamedProperty`
/// lowering.
///
/// When `has_spread` is `true` the caller observed at least one
/// `...expr` argument; the args are collected into a single Array
/// via `ArrayPush` / `SpreadIntoArray`, and the call is dispatched
/// via `CallSpread` instead of `CallProperty`.
/// M29.5: `obj.#m(args)` private-method call. Emits
/// `GetPrivateField r_recv, name_idx` for the callee (runtime
/// returns the Method closure) and dispatches through the
/// normal `CallProperty` / `CallSpread` tail with `obj` as
/// receiver.
fn lower_private_method_call<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &'a oxc_ast::ast::CallExpression<'a>,
    member: &'a oxc_ast::ast::PrivateFieldExpression<'a>,
    has_spread: bool,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = optional_member_short_circuit(ctx, member.optional)?;
    let name = member.field.name.as_str();
    enforce_private_name_declared(ctx, name, member.span)?;
    let receiver_temp = ctx.acquire_temps(1)?;
    let callee_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let (args_base, argc) = if has_spread {
        (
            ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?,
            1u16,
        )
    } else {
        let n = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
        (
            ctx.acquire_temps(n).inspect_err(|_| ctx.release_temps(2))?,
            n,
        )
    };
    let args_temp_count = argc;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // Receiver: lower `member.object` into a temp.
        lower_return_expression(builder, ctx, &member.object)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (private method receiver): {err:?}"
                ))
            })?;
        if let Some(short_circuit) = optional_short_circuit {
            emit_optional_nullish_short_circuit(builder, ctx, receiver_temp, short_circuit)?;
        }
        // Callee: GetPrivateField r_recv, name_idx — runtime
        // returns the method closure (for Method element) or
        // invokes the getter (for Accessor element) per §7.3.32.
        let idx = ctx.intern_property_name(name)?;
        builder
            .emit(
                Opcode::GetPrivateField,
                &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode GetPrivateField (private method callee): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (private method callee): {err:?}"
                ))
            })?;
        emit_call_args_and_invoke(
            builder,
            ctx,
            call,
            callee_temp,
            receiver_temp,
            args_base,
            has_spread,
        )?;
        Ok(())
    })();
    ctx.release_temps(args_temp_count);
    ctx.release_temps(2);
    lower
}

fn lower_static_method_call<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    member: &StaticMemberExpression<'a>,
    has_spread: bool,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = optional_member_short_circuit(ctx, member.optional)?;
    let receiver_temp = ctx.acquire_temps(1)?;
    let callee_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let (args_base, argc) = if has_spread {
        // One temp — holds the args-array handle.
        (
            ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?,
            1u16,
        )
    } else {
        let n = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
        (
            ctx.acquire_temps(n).inspect_err(|_| ctx.release_temps(2))?,
            n,
        )
    };
    let args_temp_count = argc;

    // M28: `super.method(args)` — the method is looked up via
    // `GetSuperProperty`, but the call receives the CURRENT
    // `this` as its receiver per §13.3.7 (SuperProperty preserves
    // `this`). So: `r_receiver` = `this`, callee pulled through
    // GetSuperProperty, then an ordinary CallProperty / CallSpread
    // dispatches against `r_receiver`.
    let super_method = matches!(&member.object, Expression::Super(_));
    let lower = (|| -> Result<(), SourceLoweringError> {
        if super_method {
            enforce_super_property_binding(ctx, &member.object)?;
            // `this` → r_receiver.
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super method): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super method receiver): {err:?}"
                    ))
                })?;
            // Callee = super.method (looked up via GetSuperProperty).
            let idx = ctx.intern_property_name(member.property.name.as_str())?;
            builder
                .emit(
                    Opcode::GetSuperProperty,
                    &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode GetSuperProperty (method callee): {err:?}"
                    ))
                })?;
        } else {
            // Receiver → r_receiver.
            lower_return_expression(builder, ctx, &member.object)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (method receiver): {err:?}"))
                })?;
            if let Some(short_circuit) = optional_short_circuit {
                emit_optional_nullish_short_circuit(builder, ctx, receiver_temp, short_circuit)?;
            }
            // Callee = receiver[name] → r_callee.
            let idx = ctx.intern_property_name(member.property.name.as_str())?;
            builder
                .emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaNamedProperty (method callee): {err:?}"
                    ))
                })?;
        }
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (method callee): {err:?}"))
            })?;
        emit_call_args_and_invoke(
            builder,
            ctx,
            call,
            callee_temp,
            receiver_temp,
            args_base,
            has_spread,
        )?;
        Ok(())
    })();
    // Release in LIFO order — args first, then (callee + receiver)
    // collapsed into a single release since the pool is just a
    // counter.
    ctx.release_temps(args_temp_count);
    ctx.release_temps(2);
    lower
}

/// Method-call path for `o[k](args)`. Key is evaluated into acc,
/// `LdaKeyedProperty` reads the callable from the receiver, and
/// the `CallProperty` emission mirrors the static-method path.
/// Receiver, key, callee, and args each occupy their own temp so
/// the evaluation order stays spec-compliant
/// (receiver → key → arguments → call). `has_spread` flips the
/// args emission + call opcode to the `CallSpread` path.
fn lower_computed_method_call<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    member: &ComputedMemberExpression<'a>,
    has_spread: bool,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = optional_member_short_circuit(ctx, member.optional)?;
    let receiver_temp = ctx.acquire_temps(1)?;
    let callee_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let (args_base, argc) = if has_spread {
        (
            ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?,
            1u16,
        )
    } else {
        let n = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
        (
            ctx.acquire_temps(n).inspect_err(|_| ctx.release_temps(2))?,
            n,
        )
    };
    let args_temp_count = argc;

    // M28: `super[k](args)` — computed super member call. Like the
    // static-method case, the receiver is the enclosing frame's
    // `this`, the callee is resolved via `GetSuperPropertyComputed`,
    // and dispatch happens through the normal CallProperty /
    // CallSpread tail.
    let super_method = matches!(&member.object, Expression::Super(_));

    let lower = (|| -> Result<(), SourceLoweringError> {
        if super_method {
            enforce_super_property_binding(ctx, &member.object)?;
            // `this` → r_receiver.
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaThis (super computed method): {err:?}"
                ))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super computed method receiver): {err:?}"
                    ))
                })?;
            // Evaluate key → acc; spill into a dedicated temp so the
            // opcode operand is a register.
            let key_temp = ctx.acquire_temps(1)?;
            let inner = (|| -> Result<(), SourceLoweringError> {
                lower_return_expression(builder, ctx, &member.expression)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (super computed key): {err:?}"
                        ))
                    })?;
                builder
                    .emit(
                        Opcode::GetSuperPropertyComputed,
                        &[
                            Operand::Reg(u32::from(receiver_temp)),
                            Operand::Reg(u32::from(key_temp)),
                        ],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode GetSuperPropertyComputed: {err:?}"
                        ))
                    })?;
                Ok(())
            })();
            ctx.release_temps(1);
            inner?;
        } else {
            // Receiver.
            lower_return_expression(builder, ctx, &member.object)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (computed method receiver): {err:?}"
                    ))
                })?;
            if let Some(short_circuit) = optional_short_circuit {
                emit_optional_nullish_short_circuit(builder, ctx, receiver_temp, short_circuit)?;
            }
            // Key → acc; LdaKeyedProperty r_receiver → acc = receiver[key].
            lower_return_expression(builder, ctx, &member.expression)?;
            builder
                .emit(
                    Opcode::LdaKeyedProperty,
                    &[Operand::Reg(u32::from(receiver_temp))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaKeyedProperty (computed callee): {err:?}"
                    ))
                })?;
        }
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (computed callee): {err:?}"))
            })?;
        emit_call_args_and_invoke(
            builder,
            ctx,
            call,
            callee_temp,
            receiver_temp,
            args_base,
            has_spread,
        )?;
        Ok(())
    })();
    ctx.release_temps(args_temp_count);
    ctx.release_temps(2);
    lower
}

/// M29: compile-time guard for `this.#x` / `obj.#x` references.
/// The private name must be declared in the immediately enclosing
/// class body (no walking of parent classes in M29 — nested-class
/// access is deferred to a future milestone).
pub(super) fn enforce_private_name_declared<'a>(
    ctx: &LoweringContext<'a>,
    name: &str,
    span: Span,
) -> Result<(), SourceLoweringError> {
    if ctx.class_private_names.iter().any(|n| n == name) {
        Ok(())
    } else {
        Err(SourceLoweringError::unsupported(
            "undeclared_private_name",
            span,
        ))
    }
}

/// §13.10.1 PrivateInExpression — lowers `#name in obj` into
/// `InPrivate r_obj, name_idx`, writing a boolean to acc. The
/// RHS is evaluated into a temp first so the operand register is
/// stable across sub-expression lowering.
pub(super) fn lower_private_in_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a oxc_ast::ast::PrivateInExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let name = expr.left.name.as_str();
    enforce_private_name_declared(ctx, name, expr.span)?;
    let obj_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(obj_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (PrivateIn obj): {err:?}"))
            })?;
        let idx = ctx.intern_property_name(name)?;
        builder
            .emit(
                Opcode::InPrivate,
                &[Operand::Reg(u32::from(obj_temp)), Operand::Idx(idx)],
            )
            .map_err(|err| SourceLoweringError::Internal(format!("encode InPrivate: {err:?}")))?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// §13.3.2 PrivateFieldExpression read — lowers `obj.#name` into
/// `GetPrivateField r_obj, name_idx`. The runtime resolves the
/// private key against `activeClosure.class_id` + the interned
/// name and throws TypeError if the target lacks the element.
pub(super) fn lower_private_field_read<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a oxc_ast::ast::PrivateFieldExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = optional_member_short_circuit(ctx, expr.optional)?;
    let name = expr.field.name.as_str();
    enforce_private_name_declared(ctx, name, expr.span)?;
    let base = materialize_member_base(builder, ctx, &expr.object)?;
    if let Some(short_circuit) = optional_short_circuit {
        emit_optional_nullish_short_circuit(builder, ctx, base.reg, short_circuit)?;
    }
    let idx = ctx.intern_property_name(name)?;
    builder
        .emit(
            Opcode::GetPrivateField,
            &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
        )
        .map_err(|err| SourceLoweringError::Internal(format!("encode GetPrivateField: {err:?}")))?;
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    Ok(())
}

/// M28: compile-time guard for `super.x` / `super[k]` references.
/// The enclosing function's `ClassSuperBinding` must both exist
/// (we're inside a class method / constructor) AND allow super
/// property access. Arrows currently do not inherit the binding,
/// so this returns `super_outside_class` for them as well.
pub(super) fn enforce_super_property_binding<'a>(
    ctx: &LoweringContext<'a>,
    super_expr: &'a Expression<'a>,
) -> Result<(), SourceLoweringError> {
    let span = super_expr.span();
    let binding = ctx
        .class_super_binding
        .ok_or_else(|| SourceLoweringError::unsupported("super_outside_class", span))?;
    if !binding.allow_super_property {
        return Err(SourceLoweringError::unsupported(
            "super_outside_class",
            span,
        ));
    }
    Ok(())
}

/// Shared emission helper for the "args + call opcode" tail of a
/// method call. Branches on `has_spread`:
///
/// - Non-spread: lowers each arg into consecutive temps starting
///   at `args_base` (via `lower_call_arguments_into_temps`) and
///   emits `CallProperty r_callee, r_receiver, RegList{args_base,
///   argc}`.
/// - Spread: treats `args_base` as a single temp holding an
///   Array. Emits `CreateArray; Star r_args; <push/spread per
///   arg>; CallSpread r_callee, r_receiver, RegList{args_base,
///   1}`. The `CallSpread` dispatch unpacks the array into
///   individual args before invoking the callable.
pub(super) fn emit_call_args_and_invoke<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    callee_temp: RegisterIndex,
    receiver_temp: RegisterIndex,
    args_base: RegisterIndex,
    has_spread: bool,
) -> Result<(), SourceLoweringError> {
    if !has_spread {
        let argc = RegisterIndex::try_from(call.arguments.len())
            .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
        lower_call_arguments_into_temps(builder, ctx, call, args_base)?;
        let call_pc = builder
            .emit(
                Opcode::CallProperty,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(receiver_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: u32::from(argc),
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CallProperty: {err:?}"))
            })?;
        ctx.attach_call_feedback(builder, call_pc);
        return Ok(());
    }

    emit_spread_call_arguments_array(builder, ctx, call, args_base)?;
    let call_pc = builder
        .emit(
            Opcode::CallSpread,
            &[
                Operand::Reg(u32::from(callee_temp)),
                Operand::Reg(u32::from(receiver_temp)),
                Operand::RegList {
                    base: u32::from(args_base),
                    count: 1,
                },
            ],
        )
        .map_err(|err| SourceLoweringError::Internal(format!("encode CallSpread: {err:?}")))?;
    ctx.attach_call_feedback(builder, call_pc);
    Ok(())
}

pub(super) fn emit_spread_call_arguments_array<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    args_base: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;

    builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("encode CreateArray (spread args): {err:?}"))
    })?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(args_base))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (spread args): {err:?}"))
        })?;
    for arg in call.arguments.iter() {
        match arg {
            Argument::SpreadElement(spread) => {
                lower_return_expression(builder, ctx, &spread.argument)?;
                builder
                    .emit(
                        Opcode::SpreadIntoArray,
                        &[Operand::Reg(u32::from(args_base))],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode SpreadIntoArray (spread arg): {err:?}"
                        ))
                    })?;
            }
            other => {
                lower_return_expression(builder, ctx, other.to_expression())?;
                builder
                    .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(args_base))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode ArrayPush (spread arg slot): {err:?}"
                        ))
                    })?;
            }
        }
    }
    Ok(())
}

/// Lowers each `CallExpression` argument into the accumulator and
/// spills it into the corresponding temp slot starting at `base`.
/// Non-spread call paths must route spread arguments through
/// `emit_spread_call_arguments_array` instead; seeing one here is
/// an internal routing bug, not a user-facing compile-surface gap.
pub(super) fn lower_call_arguments_into_temps<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    base: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;
    for (offset, arg) in call.arguments.iter().enumerate() {
        let expr = match arg {
            Argument::SpreadElement(spread) => {
                return Err(SourceLoweringError::Internal(format!(
                    "lower_call_arguments_into_temps called with spread argument at {:?}",
                    spread.span
                )));
            }
            other => other.to_expression(),
        };
        lower_return_expression(builder, ctx, expr)?;
        let slot = base
            .checked_add(RegisterIndex::try_from(offset).map_err(|_| {
                SourceLoweringError::Internal("call argument offset overflow".into())
            })?)
            .ok_or_else(|| SourceLoweringError::Internal("call argument slot overflow".into()))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (call arg): {err:?}"))
            })?;
    }
    Ok(())
}
