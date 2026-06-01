//! AST → bytecode lowering with full foundation TS erasure.
//!
//! The compiler walks the OXC AST produced by `otter-syntax` and
//! emits an [`otter_bytecode::BytecodeModule`]. After task 08 the
//! frontend handles the foundation TypeScript subset documented in the
//! mdBook frontend chapter:
//!
//! - **Erased silently** (compile to nothing): `interface`, `type`
//!   aliases, `declare` statements/functions, `import type`,
//!   `export type`, abstract methods.
//! - **Erased through** at the expression layer: `as`, `satisfies`,
//!   non-null `!`, legacy `<T>` type assertion, instantiation
//!   `f<T>` (kept transparent — operand survives).
//! - **Rejected with `TS_UNSUPPORTED` diagnostics**: `enum`,
//!   `namespace` (with runtime members), decorators. These return
//!   [`CompileError::TypeScriptUnsupported`].
//!
//! Code surface accepted at this slice: empty scripts, `undefined;`
//! statements, plus any of the above wrapped around them. Slice
//! tasks `09`–`13` add real value loading, control flow, and calls.
//!
//! # Contents
//! - [`compile_script_source`] / [`compile_script_source_to_module`] — script
//!   source entry points.
//! - [`compile_script_program`] — borrowed-AST script lowering.
//! - [`compile_module_program`] / [`compile_module_program_to_module`] —
//!   borrowed-AST ES-module lowering.
//! - [`CompileError`] — concrete error enum (`Syntax`,
//!   `TypeScriptUnsupported`, `Unsupported`).
//! - [`unwrap_ts_expr`] — strip TS-erasable expression wrappers.
//!
//! # Invariants
//! - The function table starts with `<main>` at index 0.
//! - Every emitted instruction has a matching `SpanEntry` so source
//!   spans survive into diagnostics and stack traces (foundation
//!   plan §M2).
//! - TypeScript erasure preserves the **original** spans — we never
//!   re-emit JS source and re-parse.
//!

mod assignment;
mod builtins_call;
mod builtins_table;
mod calls;
pub(crate) mod capture;
mod chain;
mod class;
mod compiled_module;
mod compiler;
mod destructuring;
mod entry;
mod errors;
mod expr;
mod for_loops;
mod function_context;
mod functions;
mod hoist;
mod module_state;
mod params;
mod scope;
mod statements;
pub(crate) mod strict_validation;
mod synthetic;
mod template;
mod try_catch;
mod ts_erasure;
mod with_statement;

use compiled_module::collect_module_metadata;
pub use compiled_module::{
    CompiledExport, CompiledImport, CompiledImportKind, CompiledModule, CompiledModuleMetadata,
    CompiledSourceSpan, LiveBindingSlot, NamedImport, ResolvedBinding,
};
pub use entry::{
    compile_module_program, compile_module_program_to_module, compile_script_program,
    compile_script_source, compile_script_source_to_module,
    compile_script_source_with_forced_strict,
};
pub use errors::CompileError;
pub use module_state::ModuleHostInfo;
pub use ts_erasure::unwrap_ts_expr;

pub(crate) use std::cell::RefCell;
pub(crate) use std::collections::{HashMap, HashSet};
pub(crate) use std::rc::Rc;

pub(crate) use assignment::*;
pub(crate) use builtins_call::*;
pub(crate) use builtins_table::*;
pub(crate) use calls::*;
pub(crate) use chain::*;
pub(crate) use class::*;
pub(crate) use compiler::Compiler;
pub(crate) use destructuring::*;
pub(crate) use entry::compile_export_inner_declaration;
pub(crate) use expr::*;
pub(crate) use for_loops::*;
pub(crate) use function_context::FunctionContext;
pub(crate) use functions::*;
pub(crate) use hoist::*;
pub(crate) use module_state::{
    ImportBinding, ModuleBuilder, ModuleState, bytecode_source_kind, find_module_import_binding,
    module_export_name_to_str, module_specifier_target,
};
pub(crate) use params::*;
pub(crate) use scope::{BindingInfo, BindingStorage, LoopFrame, Scope};
pub(crate) use statements::*;
pub(crate) use synthetic::*;
pub(crate) use template::*;
pub(crate) use try_catch::*;
pub(crate) use ts_erasure::{
    expr_kind_name, expr_span, init_to_expression, is_erased_ts_statement, rejected_ts_statement,
    stmt_kind_name, stmt_span,
};
pub(crate) use with_statement::*;

pub(crate) use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Constant, Function, Instruction,
    MappedArgumentBinding, Op, Operand, OperandList, SourceKind as BytecodeSourceKind, SpanEntry,
};
pub(crate) use otter_syntax::{
    SourceKind as SyntaxSourceKind, SyntaxDiagnostic, SyntaxError, with_program,
};
pub(crate) use oxc_ast::ast::{
    AssignmentOperator, AssignmentTarget, BinaryOperator, Expression, LogicalOperator, Program,
    SimpleAssignmentTarget, Statement, UnaryOperator, UpdateOperator,
};

#[cfg(test)]
mod tests {
    use super::*;
    use otter_syntax::with_program;

    fn host_info(specifiers: &[(&str, &str)]) -> ModuleHostInfo {
        ModuleHostInfo {
            module_url: "file:///test/main.ts".to_string(),
            resolved_imports: specifiers
                .iter()
                .map(|(s, t)| (s.to_string(), t.to_string()))
                .collect(),
        }
    }

    fn compile_module_src(src: &str, host: &ModuleHostInfo) -> BytecodeModule {
        with_program(src, SyntaxSourceKind::TypeScript, |program| {
            compile_module_program(program, SyntaxSourceKind::TypeScript, host)
        })
        .unwrap()
        .unwrap()
    }

    fn compile_module_src_err(src: &str, host: &ModuleHostInfo) -> CompileError {
        with_program(src, SyntaxSourceKind::TypeScript, |program| {
            compile_module_program(program, SyntaxSourceKind::TypeScript, host)
        })
        .unwrap()
        .unwrap_err()
    }

    fn compile_script_src(src: &str) -> BytecodeModule {
        compile_script_source(src, SyntaxSourceKind::TypeScript, "test.ts").unwrap()
    }

    fn compile_script_src_err(src: &str) -> CompileError {
        compile_script_source(src, SyntaxSourceKind::TypeScript, "test.ts").unwrap_err()
    }

    #[test]
    fn module_fragment_marks_module_init() {
        let module = compile_module_src("export let x = 7;", &host_info(&[]));
        let init = &module.functions[0];
        assert!(init.is_module);
        assert_eq!(init.name, "<module-init>");
        assert_eq!(init.module_url, "file:///test/main.ts");
        assert_eq!(init.param_count, 2);
        assert_eq!(module.module, "file:///test/main.ts");
        // Two own-upvalues for module_env + import_meta.
        assert!(init.own_upvalue_count >= 2);
    }

    #[test]
    fn module_export_mirrors_assignment() {
        let module = compile_module_src(
            "export let counter = 0; counter = counter + 1;",
            &host_info(&[]),
        );
        let init = &module.functions[0];
        // Two StoreProperty ops expected: initial declaration
        // mirror + assignment mirror.
        let store_property_count = init
            .code
            .iter()
            .filter(|i| i.op == Op::StoreProperty)
            .count();
        assert!(
            store_property_count >= 2,
            "expected >=2 StoreProperty mirrors, got {store_property_count}"
        );
    }

    #[test]
    fn module_import_lowers_to_load_import_binding() {
        let src = "import { value } from \"./other.ts\"; let y = value;";
        let host = host_info(&[("./other.ts", "file:///test/other.ts")]);
        let module = compile_module_src(src, &host);
        let init = &module.functions[0];
        // ImportNamespace at the top of the body (source record cell).
        assert!(init.code.iter().any(|i| i.op == Op::ImportNamespace));
        // §9.1.1.5 GetBindingValue — the read of `value` resolves through
        // the source module's ResolveExport table via LoadImportBinding.
        assert!(init.code.iter().any(|i| i.op == Op::LoadImportBinding));
        // module_resolutions populated from host info.
        assert_eq!(module.module_resolutions.len(), 1);
        assert_eq!(module.module_resolutions[0].specifier, "./other.ts");
        assert_eq!(module.module_resolutions[0].target, "file:///test/other.ts");
    }

    #[test]
    fn import_outside_module_mode_is_rejected() {
        let err = compile_script_src_err("import { a } from \"./x.ts\";");
        match err {
            CompileError::Unsupported { node, .. } => {
                assert!(node.contains("ImportDeclaration"), "got {node}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn dynamic_import_with_non_literal_argument_compiles() {
        // Non-literal specifiers now lower through
        // `Op::ImportNamespaceDynamic`. The runtime resolves the
        // string against the active module's resolution table.
        let src = "let s = \"./x.ts\"; import(s);";
        let module = compile_module_src(src, &host_info(&[]));
        let init = &module.functions[0];
        let dyn_count = init
            .code
            .iter()
            .filter(|i| matches!(i.op, Op::ImportNamespaceDynamic))
            .count();
        assert_eq!(dyn_count, 1, "expected one IMPORT_NAMESPACE_DYNAMIC");
    }

    #[test]
    fn script_import_defer_literal_uses_dynamic_import_path() {
        let module = compile_script_src("import.defer(\"./empty.js\");");
        let main = &module.functions[0];
        assert!(
            main.code.iter().any(|i| i.op == Op::ImportNamespaceDynamic),
            "script-mode import.defer should return a dynamic-import promise"
        );
        assert!(
            !main
                .code
                .iter()
                .any(|i| i.op == Op::ImportNamespaceDeferred),
            "script-mode import.defer must not use module-only deferred namespace lookup"
        );
    }

    #[test]
    fn module_import_defer_literal_uses_deferred_namespace_path() {
        let module = compile_module_src(
            "import.defer(\"./empty.js\");",
            &host_info(&[("./empty.js", "file:///test/empty.js")]),
        );
        let init = &module.functions[0];
        assert!(
            init.code
                .iter()
                .any(|i| i.op == Op::ImportNamespaceDeferred),
            "module-mode import.defer should resolve a deferred namespace"
        );
        assert!(
            init.code.iter().any(|i| i.op == Op::PromiseFulfilledOf),
            "module-mode import.defer should return a fulfilled promise for the deferred namespace"
        );
    }

    #[test]
    fn class_heritage_await_marks_module_init_async() {
        let src = "let foo = 1; function fn() { return function() {}; } export class C extends fn(await foo) {}";
        let module = compile_module_src(src, &host_info(&[]));
        assert!(
            module.functions[0].is_async,
            "await in class heritage is module top-level await"
        );
    }

    #[test]
    fn export_var_destructuring_with_await_compiles() {
        let src = "let foo = 1; export var name1 = await foo; export var { x = await foo } = {};";
        let module = compile_module_src(src, &host_info(&[]));
        assert!(
            module.functions[0].is_async,
            "await in export var destructuring is module top-level await"
        );
    }

    #[test]
    fn module_top_level_declaration_early_errors_are_syntax_errors() {
        for src in [
            "let x; const x = 0;",
            "let x; var x;",
            "var f; function f() {}",
            "label: { label: 0; }",
            "new.target;",
        ] {
            assert!(
                matches!(
                    compile_module_src_err(src, &host_info(&[])),
                    CompileError::Syntax { .. }
                ),
                "expected module early SyntaxError for {src}"
            );
        }
    }

    #[test]
    fn export_default_anonymous_defs_infer_default_name() {
        for src in [
            "export default (function() { return 1; });",
            "export default (function*() { yield 1; });",
            "export default class {};",
            "export default (class {});",
        ] {
            let module = compile_module_src(src, &host_info(&[]));
            assert!(
                module.functions.iter().any(|f| f.name == "default"),
                "expected default function/class name in {src}"
            );
        }
    }

    #[test]
    fn import_meta_lowers_to_load_upvalue() {
        let src = "let u = import.meta.url;";
        let module = compile_module_src(src, &host_info(&[]));
        let init = &module.functions[0];
        // The body should LoadUpvalue then LoadProperty for .url.
        let load_upvalue_count = init.code.iter().filter(|i| i.op == Op::LoadUpvalue).count();
        assert!(
            load_upvalue_count >= 1,
            "expected at least one LoadUpvalue (import.meta), got {load_upvalue_count}"
        );
        assert!(init.code.iter().any(|i| i.op == Op::LoadProperty));
    }

    #[test]
    fn bigint_literal_emits_load_bigint() {
        let module = compile_script_src("123n;");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::LoadBigInt));
        let interned = module
            .constants
            .iter()
            .any(|c| matches!(c, otter_bytecode::Constant::BigInt { decimal } if decimal == "123"));
        assert!(interned, "BigInt constant should round-trip the decimal");
    }

    #[test]
    fn bitwise_binary_ops_lower_directly() {
        let module = compile_script_src("5 & 3; 5 | 3; 5 ^ 3; 1 << 3; -1 >> 1; -1 >>> 0;");
        let main = module.main();
        for op in [
            Op::BitwiseAnd,
            Op::BitwiseOr,
            Op::BitwiseXor,
            Op::Shl,
            Op::Shr,
            Op::Ushr,
        ] {
            assert!(
                main.code.iter().any(|i| i.op == op),
                "missing {op:?} in {:?}",
                main.code
            );
        }
    }

    #[test]
    fn pow_operator_emits_pow() {
        let module = compile_script_src("2 ** 10;");
        assert!(module.main().code.iter().any(|i| i.op == Op::Pow));
    }

    #[test]
    fn compound_assign_load_modify_store() {
        let module = compile_script_src("let n = 4; n &= 1; n **= 2;");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::BitwiseAnd));
        assert!(main.code.iter().any(|i| i.op == Op::Pow));
    }

    #[test]
    fn math_namespace_inlines_constants_but_dispatches_methods_dynamically() {
        // `Math.PI` keeps its dedicated [`Op::MathLoad`] inlining,
        // while `Math.<method>(...)` calls now go through ordinary
        // property-call dispatch so user shadows are observable.
        let module = compile_script_src("Math.PI; Math.abs(-1); Math.max(1, 2, 3);");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::MathLoad));
        let calls = main
            .code
            .iter()
            .filter(|i| i.op == Op::CallMethodValue)
            .count();
        assert_eq!(calls, 2);
    }

    #[test]
    fn rest_param_marks_function_and_emits_collect_rest() {
        let module = compile_script_src("function f(...rest) { return rest.length; }");
        let f = &module.functions[1];
        assert!(f.has_rest, "rest flag should be set");
        assert_eq!(f.param_count, 0);
        assert!(f.code.iter().any(|i| i.op == Op::CollectRest));
    }

    #[test]
    fn default_param_emits_undefined_check() {
        let module = compile_script_src("function f(a, b = 5) { return a + b; }");
        let f = &module.functions[1];
        // Default lowering emits LoadUndefined + Equal + JumpIfFalse
        // before the body's normal store. Their presence is a
        // sufficient witness that the default path was taken.
        assert!(f.code.iter().any(|i| i.op == Op::LoadUndefined));
        assert!(f.code.iter().any(|i| i.op == Op::Equal));
        assert!(f.code.iter().any(|i| i.op == Op::JumpIfFalse));
    }

    #[test]
    fn array_destructure_uses_iterator_protocol() {
        let module = compile_script_src("const [a, b, ...rest] = [1, 2, 3, 4];");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::GetIterator));
        assert!(main.code.iter().any(|i| i.op == Op::IteratorNext));
        // Rest tail copies through ArrayPush.
        assert!(main.code.iter().any(|i| i.op == Op::ArrayPush));
    }

    #[test]
    fn object_destructure_loads_each_key() {
        let module = compile_script_src("function f({ x, y = 9 }) { return x + y; }");
        let f = &module.functions[1];
        // Two property loads (one per declared key), with the
        // default applied to `y`.
        let loads = f.code.iter().filter(|i| i.op == Op::LoadProperty).count();
        assert!(
            loads >= 2,
            "expected at least 2 LoadProperty ops, got {loads}: {:?}",
            f.code
        );
    }

    #[test]
    fn for_of_emits_iterator_dispatch() {
        let module = compile_script_src("for (let n of [1, 2]) { n; }");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::GetIterator));
        assert!(main.code.iter().any(|i| i.op == Op::IteratorNext));
    }

    #[test]
    fn array_literal_spread_emits_array_push_loop() {
        let module = compile_script_src("const inner = [1, 2]; [0, ...inner, 3];");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::GetIterator));
        assert!(main.code.iter().any(|i| i.op == Op::ArrayPush));
    }

    #[test]
    fn spread_call_emits_call_spread() {
        let module = compile_script_src("function f(a, b) { return a + b; } f(...[1, 2]);");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::CallSpread));
    }

    #[test]
    fn throw_statement_emits_throw_op() {
        let module = compile_script_src("throw new Error(\"x\");");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::NewError));
        assert!(main.code.iter().any(|i| i.op == Op::Throw));
    }

    #[test]
    fn try_catch_emits_enter_and_leave() {
        let module = compile_script_src("try { throw new Error(\"x\"); } catch (e) { e; }");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::EnterTry));
        assert!(main.code.iter().any(|i| i.op == Op::LeaveTry));
        // No finally → no EndFinally.
        assert!(!main.code.iter().any(|i| i.op == Op::EndFinally));
    }

    #[test]
    fn try_finally_emits_end_finally() {
        let module = compile_script_src("try { 1; } finally { 2; }");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::EnterTry));
        assert!(main.code.iter().any(|i| i.op == Op::EndFinally));
    }

    #[test]
    fn try_catch_finally_emits_two_enter_try_blocks() {
        let module =
            compile_script_src("try { throw new Error(\"x\"); } catch (e) { e; } finally { 1; }");
        let main = module.main();
        let enters = main.code.iter().filter(|i| i.op == Op::EnterTry).count();
        assert_eq!(
            enters, 2,
            "try/catch/finally should emit two EnterTry blocks: {:?}",
            main.code
        );
        assert!(main.code.iter().any(|i| i.op == Op::EndFinally));
    }

    #[test]
    fn class_declaration_emits_make_class_and_new() {
        let module = compile_script_src("class Foo { speak() { return 1; } } new Foo();");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::MakeClass));
        assert!(main.code.iter().any(|i| i.op == Op::New));
    }

    #[test]
    fn this_expression_emits_load_this() {
        let module = compile_script_src("this;");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::LoadThis),
            "expected LoadThis in {:?}",
            main.code
        );
    }

    #[test]
    fn method_call_emits_call_method_value() {
        let module = compile_script_src("const o = { v: 1 }; o.toString();");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallMethodValue),
            "expected CallMethodValue: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_call_lowers_to_call_with_this() {
        let module = compile_script_src("function f() { return this; } f.call({});");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallWithThis),
            "expected CallWithThis: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_apply_with_array_literal_unpacks() {
        let module = compile_script_src("function f(a, b) { return a + b; } f.apply({}, [1, 2]);");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallWithThis),
            "apply with literal array should lower to CallWithThis: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_apply_with_dynamic_args_uses_method_value_fallback() {
        let module = compile_script_src("function f() {} const args = [1]; f.apply({}, args);");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallMethodValue),
            "dynamic apply args should stay on method-call fallback: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_bind_emits_bind_function() {
        let module = compile_script_src("function f() {} f.bind({}, 1, 2);");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::BindFunction),
            "expected BindFunction: {:?}",
            main.code
        );
    }

    #[test]
    fn arrow_record_marked_arrow_and_emits_make_closure() {
        let module = compile_script_src("const f = () => 1; f();");
        // Arrows always go through MakeClosure (even with zero
        // captures) so the runtime can snapshot enclosing `this`.
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::MakeClosure),
            "arrow should emit MakeClosure: {:?}",
            main.code
        );
        let arrow_fn = module
            .functions
            .iter()
            .find(|f| f.is_arrow)
            .expect("arrow function record");
        assert_eq!(arrow_fn.name, "<arrow>");
    }

    #[test]
    fn closure_emits_make_closure_with_capture() {
        let module = compile_script_src(
            "function makeCounter() { let n = 0; return function() { n = n + 1; return n; }; }\nmakeCounter();",
        );
        // The inner function captures `n` from `makeCounter`, so the
        // outer body emits `MakeClosure` instead of `MakeFunction`.
        let outer = &module.functions[1];
        let has_make_closure = outer.code.iter().any(|i| i.op == Op::MakeClosure);
        assert!(
            has_make_closure,
            "outer function should emit MakeClosure for capturing inner: {:?}",
            outer.code
        );
        // The inner function reads / writes `n` through upvalue ops.
        let inner = &module.functions[2];
        assert!(
            inner.code.iter().any(|i| i.op == Op::LoadUpvalue),
            "inner should LoadUpvalue: {:?}",
            inner.code
        );
        assert!(
            inner.code.iter().any(|i| i.op == Op::StoreUpvalue),
            "inner should StoreUpvalue: {:?}",
            inner.code
        );
    }

    #[test]
    fn recursive_function_self_binding_keeps_captures() {
        let module = compile_script_src(
            "function outer() { let prefix = 'x'; function f(n) { if (n <= 0) return prefix; return f(n - 1); } return f(1); }\nouter();",
        );
        let recursive = module
            .functions
            .iter()
            .find(|f| f.name == "f")
            .expect("recursive function record");
        let self_binding = recursive
            .code
            .iter()
            .find(|i| i.op == Op::MakeClosure)
            .expect("recursive self binding should emit MakeClosure");
        assert!(
            self_binding
                .operands
                .as_slice()
                .contains(&Operand::Imm32(recursive.own_upvalue_count as i32)),
            "recursive self binding should preserve captures: {:?}",
            recursive.code
        );
    }

    #[test]
    fn empty_script_compiles() {
        let module = compile_script_src("");
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::Return);
    }

    #[test]
    fn undefined_literal_compiles() {
        let module = compile_script_src("undefined;");
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::Return);
    }

    #[test]
    fn unsupported_statement_rejects() {
        let err = compile_script_src_err("continue;");
        assert!(matches!(err, CompileError::Unsupported { .. }));
    }

    #[test]
    fn type_alias_is_erased() {
        let module = compile_script_src("type Foo = number; undefined;");
        // LoadUndefined for the body + Return.
        let main = module.main();
        assert_eq!(main.code.len(), 2);
    }

    #[test]
    fn interface_is_erased() {
        let module = compile_script_src("interface I { x: number; } undefined;");
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn declare_function_is_erased() {
        let module = compile_script_src("declare function foo(): void; undefined;");
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn import_type_is_erased() {
        let module = compile_script_src("import type { Foo } from \"./foo\"; undefined;");
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn as_expression_unwraps_to_undefined() {
        let module = compile_script_src("(undefined as any);");
        // `(undefined as any)` is statement-level; LoadUndefined + Return.
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn satisfies_expression_unwraps_to_undefined() {
        let module = compile_script_src("(undefined satisfies unknown);");
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn non_null_unwraps_to_undefined() {
        let module = compile_script_src("undefined!;");
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn enum_is_rejected_with_ts_unsupported() {
        let err = compile_script_src_err("enum E { A }");
        match err {
            CompileError::TypeScriptUnsupported { node, .. } => {
                assert_eq!(node, "TSEnumDeclaration");
            }
            other => panic!("expected TypeScriptUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn namespace_with_runtime_body_is_rejected() {
        let err = compile_script_src_err("namespace N { export const x = 1; }");
        assert!(matches!(err, CompileError::TypeScriptUnsupported { .. }));
    }

    #[test]
    fn declared_namespace_is_erased() {
        let module = compile_script_src("declare namespace N { function f(): void; } undefined;");
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn string_literal_compiles_to_load_string() {
        // Parenthesize to keep OXC from treating the bare literal
        // as a directive prologue.
        let module = compile_script_src("(\"abc\");");
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadString);
        assert_eq!(main.code[1].op, Op::Return);
        assert_eq!(module.constants.len(), 1);
        let Constant::String { utf16 } = &module.constants[0] else {
            panic!("expected String constant");
        };
        assert_eq!(utf16, &vec![b'a' as u16, b'b' as u16, b'c' as u16]);
    }

    #[test]
    fn string_concat_compiles_to_add() {
        let module = compile_script_src("\"a\" + \"b\";");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::Add));
    }

    #[test]
    fn strict_equals_compiles_to_eq() {
        let module = compile_script_src("\"a\" === \"a\";");
        assert!(module.main().code.iter().any(|i| i.op == Op::Equal));
    }

    #[test]
    fn numeric_literal_smi_compiles_to_load_int32() {
        let module = compile_script_src("(42);");
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadInt32));
    }

    #[test]
    fn arithmetic_lowers_to_numeric_ops() {
        let module = compile_script_src("1 + 2 * 3 - 4 / 5;");
        let ops: Vec<Op> = module.main().code.iter().map(|i| i.op).collect();
        assert!(ops.contains(&Op::Add));
        assert!(ops.contains(&Op::Sub));
        assert!(ops.contains(&Op::Mul));
        assert!(ops.contains(&Op::Div));
    }

    #[test]
    fn unary_minus_lowers_to_neg() {
        let module = compile_script_src("-(5);");
        assert!(module.main().code.iter().any(|i| i.op == Op::Neg));
    }

    #[test]
    fn boolean_literal_lowers() {
        let module = compile_script_src("(true);");
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadTrue));
    }

    #[test]
    fn dot_length_compiles_to_load_property() {
        // Slice 17 generalised `.length` into the same
        // `LoadProperty` opcode used for object property access;
        // the runtime keeps the string-length fast path inside
        // the dispatcher.
        let module = compile_script_src("\"abc\".length;");
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadProperty));
    }

    #[test]
    fn template_no_interpolation_compiles_to_load_string() {
        let module = compile_script_src("`abc`;");
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadString));
    }

    #[test]
    fn duplicate_string_literals_share_constant() {
        let module = compile_script_src("(\"abc\"); (\"abc\");");
        assert_eq!(module.constants.len(), 1);
    }
}
