//! Call admission and source-ordered identifier operation tests.
//!
//! # Contents
//! - Mutable exposure, coercion and derived-this refusal.
//! - Private registers, immutable bindings and primitive unary admission.
//! - Cross-clause and eval-chain TDZ admission and checked storage.
//! - Prepared with/eval targets and immutable update results.
//!
//! # Invariants
//! A split call loads its exact method before the argument operation. A fused
//! call has no separate method load and retains the same receiver and arguments.
//! Every fixture passes bytecode and context-chain verification.
//!
//! # See also
//! - `calls` owns method-lookup commutation proofs.
//! - `context_tests` validates the emitted binding paths.

use crate::{SyntaxSourceKind, compile_script_source};
use otter_bytecode::{BytecodeModule, Constant, Function, Op, Operand};

fn compile(source: &str) -> BytecodeModule {
    let module = compile_script_source(source, SyntaxSourceKind::TypeScript, "method-order.ts")
        .expect("method-call proof fixture");
    crate::context_tests::assert_verified(&module);
    module
}

fn function<'a>(module: &'a BytecodeModule, name: &str) -> &'a Function {
    module
        .functions
        .iter()
        .find(|function| function.name == name)
        .expect("fixture function")
}

fn is_method_name(module: &BytecodeModule, operand: Option<Operand>) -> bool {
    is_string(module, operand, "m")
}

fn is_string(module: &BytecodeModule, operand: Option<Operand>, expected: &str) -> bool {
    let Some(Operand::ConstIndex(index)) = operand else {
        return false;
    };
    matches!(module.constants.get(index as usize), Some(Constant::String { utf16 })
        if utf16.iter().copied().eq(expected.encode_utf16()))
}

fn assert_split(module: &BytecodeModule, name: &str, argument_ops: &[Op]) {
    let function = function(module, name);
    let code = &function.code;
    let (lookup_pc, lookup) = code
        .iter()
        .enumerate()
        .find(|(_, instruction)| {
            instruction.op == Op::LoadProperty
                && is_method_name(module, code.operand(instruction, 2))
        })
        .expect("explicit method lookup");
    let callee = code.operand(lookup, 0);
    let call_pc = code
        .iter()
        .enumerate()
        .find(|(_, instruction)| {
            instruction.op == Op::CallWithThis && code.operand(instruction, 1) == callee
        })
        .map(|(pc, _)| pc)
        .expect("call of the loaded method");
    assert!(
        code.iter().enumerate().any(|(pc, instruction)| {
            lookup_pc < pc && pc < call_pc && argument_ops.contains(&instruction.op)
        }),
        "argument operation must follow lookup: {:?}",
        code
    );
    assert!(!code.iter().any(|instruction| {
        instruction.op == Op::CallMethodValue
            && is_method_name(module, code.operand(instruction, 2))
    }));
}

fn assert_fused(module: &BytecodeModule, name: &str) {
    let code = &function(module, name).code;
    assert!(code.iter().any(|instruction| {
        instruction.op == Op::CallMethodValue
            && is_method_name(module, code.operand(instruction, 2))
    }));
    assert!(!code.iter().any(|instruction| {
        instruction.op == Op::LoadProperty && is_method_name(module, code.operand(instruction, 2))
    }));
}

#[test]
fn mutable_exposure_and_tdz_reads_follow_method_lookup() {
    for source in [
        "function test(o) { let value=1; const write=()=>value=9; o.m(value); return write; }",
        "function test(o,value) { globalThis.saved=arguments; o.m(value); }",
        "function test(o) { let value=1; eval('value=9'); o.m(value); }",
    ] {
        assert_split(
            &compile(source),
            "test",
            &[Op::LoadContextSlot, Op::LoadContextSlotChecked],
        );
    }
    assert_split(
        &compile("function test(o) { o.m(value); let value=1; }"),
        "test",
        &[Op::TdzError],
    );
}

#[test]
fn unknown_or_throwing_numeric_coercion_follows_method_lookup() {
    for argument in ["+value", "-value", "~value", "+1n", "+(-1n)"] {
        let source = format!("function test(o,value:number) {{ o.m({argument}); }}");
        assert_split(
            &compile(&source),
            "test",
            &[Op::ToNumber, Op::Neg, Op::BitwiseNot],
        );
    }
}

#[test]
fn derived_this_and_lexical_arrows_follow_method_lookup() {
    assert_split(
        &compile(
            "class B {} class D extends B { constructor() { const init=()=>super(); \
             const o={get m(){init();return x=>x;}}; o.m(this); } }",
        ),
        "D",
        &[Op::LoadContextSlotChecked],
    );
    assert_split(
        &compile(
            "class B {} class D extends B { constructor(o) { const invoke=()=>o.m(this); \
             invoke(); } }",
        ),
        "invoke",
        &[Op::LoadContextSlotChecked],
    );
}

#[test]
fn stable_values_retain_fused_method_calls() {
    for source in [
        "function test(o,value) { let local=value; o.m(local); }",
        "function test(o,value) { o.m(value); } function observer(){return test.arguments;}",
        "function test(o) { 'use strict'; o.m(this); }",
        "function test(o) { const value=1; const read=()=>value; o.m(value); return read; }",
    ] {
        assert_fused(&compile(source), "test");
    }
    for argument in [
        "-1",
        "+'3'",
        "~1n",
        "-1n",
        "+(!value)",
        "void value",
        "!value",
    ] {
        let source = format!("function test(o,value) {{ o.m({argument}); }}");
        assert_fused(&compile(&source), "test");
    }
}

#[test]
fn switch_lexical_arguments_read_after_the_method_lookup() {
    for declaration in ["const value=1;", "const value=1; const keep=()=>value;"] {
        let source = format!(
            "function test(o,mode) {{ switch(mode) {{ case 0: {declaration} break; \
             case 1: o.m(value); }} }}"
        );
        assert_split(&compile(&source), "test", &[Op::LoadContextSlotChecked]);
    }
    assert_split(
        &compile(
            "function test(o,mode) { switch(mode) { case 0: const value=1; break; \
             case 1: const invoke=function inner(){o.m(value);}; invoke(); } }",
        ),
        "inner",
        &[Op::LoadContextSlotChecked],
    );
}

#[test]
fn caller_chain_const_metadata_does_not_prove_a_live_initialized_value() {
    use otter_bytecode::{
        EvalCallerChain, EvalCallerScope, ScopeDescriptor, ScopeFlags, ScopeKind, SlotDescriptor,
        SlotKind,
    };
    let chain = EvalCallerChain {
        scopes: vec![EvalCallerScope {
            descriptor: ScopeDescriptor {
                kind: ScopeKind::Body,
                flags: ScopeFlags {
                    strict: false,
                    var_scope: true,
                    has_extension: false,
                },
                slots: vec![
                    SlotDescriptor {
                        name: "o".to_string(),
                        kind: SlotKind::Let,
                        exported: false,
                    },
                    SlotDescriptor {
                        name: "value".to_string(),
                        kind: SlotKind::Const,
                        exported: false,
                    },
                ],
            }
            .into(),
            extension_names: Vec::new(),
        }],
        var_depth: Some(0),
    };
    let module = crate::compile_eval_source(
        "o.m(value)",
        SyntaxSourceKind::JavaScript,
        "method-eval",
        false,
        false,
        Some(&chain),
        true,
        false,
        false,
        false,
        false,
        crate::CompileOptions::default(),
    )
    .expect("caller-chain method fixture");
    crate::context_tests::assert_verified_with_caller(&module, Some(&chain));
    assert_split(&module, "<main>", &[Op::LoadContextSlotChecked]);
}

#[test]
fn cross_clause_read_update_and_assignment_use_one_checked_slot() {
    use otter_bytecode::ScopeKind;
    for (declaration, operation, checked_op) in [
        (
            "let value=1;",
            "consume(value);",
            Op::LoadContextSlotChecked,
        ),
        ("let value=1;", "value++;", Op::LoadContextSlotChecked),
        ("let value=1;", "value+=2;", Op::LoadContextSlotChecked),
        ("let value=1;", "value=2;", Op::StoreContextSlotChecked),
        ("let value=1;", "[value]=[2];", Op::StoreContextSlotChecked),
        (
            "let value=1;",
            "({value}=source);",
            Op::StoreContextSlotChecked,
        ),
        (
            "let value=1;",
            "({[key()]: value}=source);",
            Op::StoreContextSlotChecked,
        ),
        ("const value=1;", "value++;", Op::LoadContextSlotChecked),
        ("const value=1;", "value=2;", Op::LoadContextSlotChecked),
        ("const value=1;", "[value]=[2];", Op::LoadContextSlotChecked),
        (
            "const value=1;",
            "({[key()]: value}=source);",
            Op::LoadContextSlotChecked,
        ),
    ] {
        let source = format!(
            "function test(mode,consume,source) {{ switch(mode) {{ \
             case 0: {declaration} break; case 1: {operation} }} }}"
        );
        let module = compile(&source);
        let test = function(&module, "test");
        assert!(
            test.scopes.iter().any(|scope| {
                scope.kind == ScopeKind::Switch
                    && scope.slots.iter().any(|slot| slot.name == "value")
            }),
            "cross-clause binding must retain a live TDZ hole: {operation}"
        );
        let checked_pc = test
            .code
            .iter()
            .position(|instruction| instruction.op == checked_op)
            .unwrap_or_else(|| panic!("checked operation for {operation}: {:?}", test.code));
        if declaration.starts_with("const") {
            let error_pc = test
                .code
                .iter()
                .enumerate()
                .find(|(pc, instruction)| {
                    instruction.op == Op::NewError
                        && is_string(&module, test.code.operand(instruction, 1), "TypeError")
                        && test.code.iter().enumerate().any(|(message_pc, message)| {
                            message_pc < *pc
                                && message.op == Op::LoadString
                                && test.code.operand(message, 0)
                                    == test.code.operand(instruction, 2)
                                && is_string(
                                    &module,
                                    test.code.operand(message, 1),
                                    "Assignment to constant variable 'value'.",
                                )
                        })
                })
                .map(|(pc, _)| pc)
                .expect("immutable assignment error");
            assert!(
                checked_pc < error_pc,
                "TDZ precedes immutable error for {operation}"
            );
        }
    }
}

#[test]
fn clause_private_lexicals_retain_register_updates_and_fused_calls() {
    let module = compile(
        "function test(o,mode) { switch(mode) { \
         case 0: const value=1; o.m(value); break; \
         case 1: let privateValue=2; privateValue++; o.m(privateValue); break; } }",
    );
    let test = function(&module, "test");
    assert_fused(&module, "test");
    assert!(!test.code.iter().any(|instruction| matches!(
        instruction.op,
        Op::CreateContext | Op::LoadContextSlotChecked | Op::StoreContextSlotChecked
    )));
    assert!(!test.scopes.iter().any(|scope| {
        scope
            .slots
            .iter()
            .any(|slot| slot.name == "value" || slot.name == "privateValue")
    }));
}

#[test]
fn destructuring_with_targets_are_acquired_before_values_and_defaults() {
    for (pattern, value, extraction) in [
        ("[value=make()]", "source", Op::IteratorNext),
        ("({value=make()})", "source", Op::LoadProperty),
        ("({x:value=make()})", "source", Op::LoadProperty),
        ("[...value]", "source", Op::IteratorNext),
        ("({...value})", "source", Op::CopyDataProperties),
    ] {
        let pattern =
            if let Some(inner) = pattern.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
                inner
            } else {
                pattern
            };
        let source = format!(
            "function test(source,target,make) {{ const value=1; \
             with(target) {{ ({pattern}={value}); }} }}"
        );
        let module = compile(&source);
        let code = &function(&module, "test").code;
        let acquisition = code
            .iter()
            .enumerate()
            .find(|(pc, instruction)| {
                instruction.op == Op::HasProperty
                    && code.iter().enumerate().any(|(key_pc, key)| {
                        key_pc < *pc
                            && key.op == Op::LoadString
                            && code.operand(key, 0) == code.operand(instruction, 1)
                            && is_string(&module, code.operand(key, 1), "value")
                    })
            })
            .map(|(pc, _)| pc)
            .expect("with reference acquisition");
        let source_reg = code
            .iter()
            .enumerate()
            .rev()
            .find(|(pc, instruction)| *pc < acquisition && instruction.op == Op::JumpIfNullish)
            .and_then(|(_, instruction)| code.operand(instruction, 1))
            .expect("destructuring source nullish guard");
        let extraction = code
            .iter()
            .enumerate()
            .find(|(pc, instruction)| {
                *pc > acquisition
                    && instruction.op == extraction
                    && (instruction.op != Op::LoadProperty
                        || code.operand(instruction, 1) == Some(source_reg))
            })
            .map(|(pc, _)| pc)
            .expect("value extraction");
        let store = code
            .iter()
            .position(|instruction| instruction.op == Op::StoreProperty)
            .expect("store to selected with object");
        assert!(
            acquisition < extraction && extraction < store,
            "reference, extraction, PutValue order for {pattern}: {:?}",
            code
        );
        if pattern.contains("make()") {
            let default = code
                .iter()
                .enumerate()
                .find(|(pc, instruction)| {
                    *pc > extraction && matches!(instruction.op, Op::Call | Op::CallWithThis)
                })
                .map(|(pc, _)| pc)
                .expect("default initializer");
            assert!(extraction < default && default < store);
        }
    }
}

#[test]
fn destructuring_eval_references_are_pinned_before_default_effects() {
    let module = compile(
        "function outer() { var value=1; return function test(source) { eval(''); \
         [value=eval('var value=9; 3')]=source; return value; }; }",
    );
    let code = &function(&module, "test").code;
    let acquire = code
        .iter()
        .position(|instruction| instruction.op == Op::ResolveLookupRef)
        .expect("resolved environment reference");
    let extract = code
        .iter()
        .position(|instruction| instruction.op == Op::IteratorNext)
        .expect("iterator value");
    let store = code
        .iter()
        .position(|instruction| instruction.op == Op::StoreRef)
        .expect("store through resolved environment");
    let default = code
        .iter()
        .enumerate()
        .find(|(pc, instruction)| *pc > extract && instruction.op == Op::Eval)
        .map(|(pc, _)| pc)
        .expect("default initializer can extend the local environment");
    assert!(acquire < extract && extract < default && default < store);
}

#[test]
fn sloppy_function_self_update_returns_prefix_delta_without_a_store() {
    for (expression, delta) in [("++self", 1), ("--self", -1)] {
        let module = compile(&format!(
            "function outer() {{ return function self(){{return {expression};}}; }}"
        ));
        let code = &function(&module, "self").code;
        let (numeric_pc, numeric) = code
            .iter()
            .enumerate()
            .find(|(_, instruction)| instruction.op == Op::ToNumeric)
            .expect("one observable numeric coercion");
        let increment = code
            .iter()
            .find(|instruction| instruction.op == Op::Increment)
            .expect("prefix numeric result");
        assert_eq!(code.operand(increment, 1), code.operand(numeric, 0));
        assert_eq!(code.operand(increment, 2), Some(Operand::Imm32(delta)));
        let (return_pc, returned) = code
            .iter()
            .enumerate()
            .find(|(_, instruction)| instruction.op == Op::ReturnValue)
            .expect("returned prefix result");
        assert_eq!(code.operand(returned, 0), code.operand(increment, 0));
        assert!(!code.iter().enumerate().any(|(pc, instruction)| {
            numeric_pc < pc
                && pc < return_pc
                && matches!(
                    instruction.op,
                    Op::StoreContextSlot | Op::StoreContextSlotChecked | Op::NewError
                )
        }));
    }
    for expression in ["self++", "self--"] {
        let module = compile(&format!(
            "function outer() {{ return function self(){{return {expression};}}; }}"
        ));
        let code = &function(&module, "self").code;
        let numeric = code
            .iter()
            .find(|instruction| instruction.op == Op::ToNumeric)
            .expect("postfix numeric value");
        let returned = code
            .iter()
            .find(|instruction| instruction.op == Op::ReturnValue)
            .expect("returned postfix result");
        assert_eq!(code.operand(returned, 0), code.operand(numeric, 0));
        let increment = code
            .iter()
            .find(|instruction| instruction.op == Op::Increment)
            .expect("postfix delta precedes the ignored store");
        assert_eq!(code.operand(increment, 1), code.operand(numeric, 0));
    }
}

#[test]
fn immutable_delta_precedes_rejection_even_for_discarded_results() {
    for expression in ["value++", "--value", "++self", "self--"] {
        let source = if expression.contains("self") {
            format!(
                "function outer() {{ return function self(){{ 'use strict'; {expression}; }}; }}"
            )
        } else {
            format!("function test() {{ const value=1n; {expression}; }}")
        };
        let module = compile(&source);
        let code = &function(
            &module,
            if expression.contains("self") {
                "self"
            } else {
                "test"
            },
        )
        .code;
        let numeric = code
            .iter()
            .position(|instruction| instruction.op == Op::ToNumeric)
            .expect("numeric value");
        let increment = code
            .iter()
            .position(|instruction| instruction.op == Op::Increment)
            .expect("BigInt-capable delta");
        let rejection = code
            .iter()
            .position(|instruction| instruction.op == Op::NewError)
            .expect("immutable PutValue rejection");
        assert!(numeric < increment && increment < rejection);
        assert_eq!(
            code.iter()
                .filter(|instruction| instruction.op == Op::Increment)
                .count(),
            1
        );
    }
    let module = compile("function outer(){return function self(){++self;self--;};}");
    let code = &function(&module, "self").code;
    assert_eq!(
        code.iter()
            .filter(|instruction| instruction.op == Op::ToNumeric)
            .count(),
        2
    );
    assert_eq!(
        code.iter()
            .filter(|instruction| instruction.op == Op::Increment)
            .count(),
        2
    );
    assert!(
        !code
            .iter()
            .any(|instruction| instruction.op == Op::NewError)
    );
}

#[test]
fn logical_with_assignment_uses_the_same_selected_object() {
    for operation in ["||=", "&&=", "??="] {
        let module = compile(&format!(
            "function test(target,make) {{ const value=1; with(target) {{ value {operation} make(); }} }}"
        ));
        let code = &function(&module, "test").code;
        let read = code
            .iter()
            .position(|instruction| {
                instruction.op == Op::LoadProperty
                    && is_string(&module, code.operand(instruction, 2), "value")
            })
            .expect("selected with object read");
        let store = code
            .iter()
            .find(|instruction| instruction.op == Op::StoreProperty)
            .expect("selected with object store");
        // The first value lookup may inspect unscopables; GetBindingValue's
        // load uses the same saved receiver as PutValue.
        let receiver = code.operand(store, 0);
        assert!(code.iter().enumerate().any(|(pc, instruction)| {
            pc >= read
                && instruction.op == Op::LoadProperty
                && code.operand(instruction, 1) == receiver
                && is_string(&module, code.operand(instruction, 2), "value")
        }));
    }
}

#[test]
fn logical_unresolvable_reads_throw_before_rhs_in_sloppy_code() {
    for operation in ["||=", "&&=", "??="] {
        let module = compile(&format!(
            "function test(make) {{ missingLogical {operation} make(); }}"
        ));
        let code = &function(&module, "test").code;
        let read = code
            .iter()
            .position(|instruction| instruction.op == Op::LoadGlobalOrThrow)
            .expect("GetValue rejects the unresolved reference");
        let rhs = code
            .iter()
            .position(|instruction| matches!(instruction.op, Op::Call | Op::CallWithThis))
            .expect("right-hand-side invocation");
        assert!(read < rhs);
        assert!(
            !code
                .iter()
                .any(|instruction| instruction.op == Op::LoadGlobalOrUndefined)
        );
    }
}

#[test]
fn with_unscopables_object_proof_includes_functions_and_excludes_nullish() {
    let module = compile("function test(target) { let value=1; with(target) { value=9; } }");
    let code = &function(&module, "test").code;
    for kind in ["object", "function"] {
        assert!(
            code.iter()
                .any(|instruction| instruction.op == Op::LoadString
                    && is_string(&module, code.operand(instruction, 1), kind))
        );
    }
    let nullish = code
        .iter()
        .position(|instruction| instruction.op == Op::JumpIfNullish)
        .expect("null is not an unscopables object");
    let type_of = code
        .iter()
        .position(|instruction| instruction.op == Op::TypeOf)
        .expect("object and callable type proof");
    assert!(nullish < type_of);
}

#[test]
fn strict_global_snapshot_is_guarded_by_selected_with_and_eval_references() {
    for (source, with_hit, eval_hit) in [
        (
            "function outer(target,make) { with(target) { return function strict(){ \
             'use strict'; missingStrict=make(); }; } }",
            true,
            false,
        ),
        (
            "function outer(make) { eval(''); return function strict(){ \
             'use strict'; missingStrict=make(); }; }",
            false,
            true,
        ),
        (
            "function outer(target,make) { eval(''); with(target) { return function strict(){ \
             'use strict'; missingStrict=make(); }; } }",
            true,
            true,
        ),
    ] {
        let module = compile(source);
        let code = &function(&module, "strict").code;
        let branch_target = |pc: usize| {
            let instruction = code.get(pc).expect("branch instruction");
            let Some(Operand::Imm32(offset)) = code.operand(instruction, 0) else {
                panic!("branch offset")
            };
            usize::try_from(pc as i64 + 1 + i64::from(offset)).expect("forward branch target")
        };
        let (snapshot_pc, snapshot) = code
            .iter()
            .enumerate()
            .find(|(_, instruction)| {
                instruction.op == Op::GlobalBindingExists
                    && is_string(&module, code.operand(instruction, 1), "missingStrict")
            })
            .expect("selected global existence proof");
        let rhs_pc = code
            .iter()
            .position(|instruction| matches!(instruction.op, Op::Call | Op::CallWithThis))
            .expect("RHS effects");
        assert!(snapshot_pc < rhs_pc);
        let (global_store_pc, global_store) = code
            .iter()
            .enumerate()
            .find(|(_, instruction)| {
                instruction.op == Op::StoreGlobalChecked
                    && is_string(&module, code.operand(instruction, 1), "missingStrict")
            })
            .expect("store consumes the pre-effect proof");
        assert_eq!(code.operand(global_store, 2), code.operand(snapshot, 0));
        assert!(rhs_pc < global_store_pc);
        if with_hit {
            assert!(
                code.iter().enumerate().any(|(pc, instruction)| {
                    pc < snapshot_pc
                        && instruction.op == Op::JumpIfTrue
                        && snapshot_pc < branch_target(pc)
                        && branch_target(pc) <= rhs_pc
                }),
                "a with hit must skip the global snapshot: {:?}",
                code
            );
        }
        if eval_hit {
            let resolve = code
                .iter()
                .find(|instruction| instruction.op == Op::ResolveLookupRef)
                .expect("saved eval extension reference");
            let reference = code.operand(resolve, 0);
            let snapshot_guard = code
                .iter()
                .enumerate()
                .find(|(pc, instruction)| {
                    *pc < snapshot_pc
                        && instruction.op == Op::JumpIfNullish
                        && code.operand(instruction, 1) == reference
                })
                .map(|(pc, _)| pc)
                .expect("only an extension miss consults globals");
            assert_eq!(branch_target(snapshot_guard), snapshot_pc);
            assert_eq!(
                code.get(snapshot_guard + 1)
                    .expect("extension-hit branch")
                    .op,
                Op::Jump
            );
            assert!(snapshot_pc < branch_target(snapshot_guard + 1));
            let (extension_store_pc, extension_store) = code
                .iter()
                .enumerate()
                .find(|(_, instruction)| {
                    instruction.op == Op::StoreRef && code.operand(instruction, 1) == reference
                })
                .expect("an extension hit stores through the retained reference");
            let store_guard = code
                .iter()
                .enumerate()
                .find(|(pc, instruction)| {
                    rhs_pc < *pc
                        && *pc < extension_store_pc
                        && instruction.op == Op::JumpIfNullish
                        && code.operand(instruction, 1) == reference
                })
                .map(|(pc, _)| pc)
                .expect("extension miss selects checked global PutValue");
            assert_eq!(branch_target(store_guard), global_store_pc);
            assert_eq!(code.operand(extension_store, 1), reference);
        }
    }
}
