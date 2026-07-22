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

mod annex_b;
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
mod type_hints;
mod with_statement;

use compiled_module::collect_module_metadata;
pub use compiled_module::{
    CompiledExport, CompiledImport, CompiledImportKind, CompiledModule, CompiledModuleMetadata,
    CompiledSourceSpan, LiveBindingSlot, NamedImport, ResolvedBinding,
};
pub use entry::{
    EvalCallerBinding, compile_eval_source, compile_module_program,
    compile_module_program_to_module, compile_script_program, compile_script_source,
    compile_script_source_to_module, compile_script_source_with_forced_strict,
    compile_script_source_with_top_level_await,
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
pub(crate) use type_hints::{TypeHint, annotation_hint, expr_number_typed};
pub(crate) use with_statement::*;

pub(crate) use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Constant, Function,
    FunctionCodeBuilder, MappedArgumentBinding, Op, Operand, SourceKind as BytecodeSourceKind,
    SpanEntry,
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
        // A module early error can surface either at parse time (`with_program`
        // returns a `SyntaxError`, e.g. a top-level `new.target`) or during
        // lowering (`compile_module_program` returns a `CompileError`). Fold the
        // parse-level case into `CompileError::Syntax` so both are reported the
        // same way, matching the script entry points.
        match with_program(src, SyntaxSourceKind::TypeScript, |program| {
            compile_module_program(program, SyntaxSourceKind::TypeScript, host)
        }) {
            Ok(result) => result.unwrap_err(),
            Err(syntax) => CompileError::from(syntax),
        }
    }

    fn compile_script_src(src: &str) -> BytecodeModule {
        compile_script_source(src, SyntaxSourceKind::TypeScript, "test.ts").unwrap()
    }

    fn compile_script_src_err(src: &str) -> CompileError {
        compile_script_source(src, SyntaxSourceKind::TypeScript, "test.ts").unwrap_err()
    }

    fn compile_tsx_script_src(src: &str) -> BytecodeModule {
        compile_script_source(src, SyntaxSourceKind::TypeScriptJsx, "test.tsx").unwrap()
    }

    fn string_constants(module: &BytecodeModule) -> Vec<String> {
        module
            .constants
            .iter()
            .filter_map(|constant| match constant {
                Constant::String { utf16 } => Some(String::from_utf16(utf16).unwrap()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn register_var_hoist_reuses_undefined_frame_initialization() {
        let module = compile_script_src("function f() { return value; var value; }");
        let function = module
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("compiled f");

        assert!(
            !function
                .code
                .iter()
                .any(|instruction| matches!(instruction.op, Op::LoadUndefined | Op::StoreLocal)),
            "register-backed var hoist should not rewrite an already-undefined frame slot"
        );
    }

    #[test]
    fn captured_var_hoist_still_initializes_its_upvalue_cell() {
        let module = compile_script_src(
            "function f() { function read() { return value; } return read; var value; }",
        );
        let function = module
            .functions
            .iter()
            .find(|function| function.name == "f")
            .expect("compiled f");

        assert!(
            function
                .code
                .iter()
                .any(|instruction| instruction.op == Op::LoadUndefined)
        );
        assert!(
            function
                .code
                .iter()
                .any(|instruction| instruction.op == Op::StoreUpvalue)
        );
    }

    #[test]
    fn simple_formals_alias_their_incoming_registers() {
        let module =
            compile_script_src("function add(left, right) { return left + right; } add(1, 2);");
        let function = module
            .functions
            .iter()
            .find(|function| function.name == "add")
            .expect("compiled add");

        assert!(
            !function
                .code
                .iter()
                .any(|instruction| instruction.op == Op::StoreLocal),
            "uncaptured simple formals should need no prologue copies"
        );
    }

    #[test]
    fn binary_initializer_writes_its_register_binding_directly() {
        let module =
            compile_script_src("function add(left, right) { var sum = left + right; return sum; }");
        let function = module
            .functions
            .iter()
            .find(|function| function.name == "add")
            .expect("compiled add");

        assert_eq!(
            function
                .code
                .iter()
                .filter(|instruction| instruction.op == Op::StoreLocal)
                .count(),
            0,
            "binary initializer should write the binding register directly"
        );
    }

    #[test]
    fn branch_initializers_keep_one_shared_destination() {
        let module = compile_script_src(
            "function choose(flag, left, right) {
                let logical = flag && left;
                let conditional = flag ? logical : right;
                return conditional;
            }",
        );
        let function = module
            .functions
            .iter()
            .find(|function| function.name == "choose")
            .expect("compiled choose");

        assert_eq!(
            function
                .code
                .iter()
                .filter(|instruction| instruction.op == Op::StoreLocal)
                .count(),
            4,
            "logical and conditional branches should write only their shared destinations"
        );
    }

    #[test]
    fn captured_formal_still_initializes_its_upvalue_cell() {
        let module =
            compile_script_src("function capture(value) { return () => value; } capture(1);");
        let function = module
            .functions
            .iter()
            .find(|function| function.name == "capture")
            .expect("compiled capture");

        assert!(
            function
                .code
                .iter()
                .any(|instruction| instruction.op == Op::StoreUpvalue)
        );
    }

    #[test]
    fn jsx_element_lowers_to_react_create_element() {
        let module = compile_tsx_script_src("const x = <div id=\"a\">hi</div>;");
        let main = &module.functions[0];
        assert!(main.code.iter().any(|i| i.op == Op::CallWithThis));
        assert!(main.code.iter().any(|i| i.op == Op::DefineDataProperty));
        let strings = string_constants(&module);
        for expected in ["React", "createElement", "div", "id", "a", "hi"] {
            assert!(
                strings.iter().any(|value| value == expected),
                "missing string constant {expected:?}; got {strings:?}"
            );
        }
    }

    #[test]
    fn jsx_fragment_lowers_to_react_fragment() {
        let module = compile_tsx_script_src("const x = <>hi</>;");
        let main = &module.functions[0];
        assert!(main.code.iter().any(|i| i.op == Op::CallWithThis));
        let strings = string_constants(&module);
        for expected in ["React", "Fragment", "createElement", "hi"] {
            assert!(
                strings.iter().any(|value| value == expected),
                "missing string constant {expected:?}; got {strings:?}"
            );
        }
    }

    #[test]
    fn jsx_spread_props_preserve_copy_data_properties() {
        let module = compile_tsx_script_src("const props = {}; const x = <Comp {...props} ok />;");
        let main = &module.functions[0];
        assert!(main.code.iter().any(|i| i.op == Op::CopyDataProperties));
        assert!(main.code.iter().any(|i| i.op == Op::DefineDataProperty));
        let strings = string_constants(&module);
        for expected in ["Comp", "ok", "React", "createElement"] {
            assert!(
                strings.iter().any(|value| value == expected),
                "missing string constant {expected:?}; got {strings:?}"
            );
        }
    }

    #[test]
    fn module_fragment_marks_module_init() {
        let module = compile_module_src("export let x = 7;", &host_info(&[]));
        let init = &module.functions[0];
        assert!(init.is_module);
        assert_eq!(init.name, "<module-init>");
        assert_eq!(init.module_url, "file:///test/main.ts");
        // module_env, import_meta, link-phase flag.
        assert_eq!(init.param_count, 3);
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
    fn math_namespace_inlines_constants_and_guards_methods() {
        // `Math.PI` keeps its dedicated [`Op::MathLoad`] inlining,
        // while `Math.<method>(...)` calls use guarded [`Op::MathCall`]
        // dispatch so runtime user shadows remain observable.
        let module = compile_script_src("Math.PI; Math.abs(-1); Math.max(1, 2, 3);");
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::MathLoad));
        let calls = main.code.iter().filter(|i| i.op == Op::MathCall).count();
        assert_eq!(calls, 2);
    }

    #[test]
    fn lexical_math_shadow_keeps_ordinary_call() {
        let module = compile_script_src("let Math = { sqrt() { return 7; } }; Math.sqrt(4);");
        let main = module.main();
        assert!(!main.code.iter().any(|i| i.op == Op::MathCall));
        assert!(main.code.iter().any(|i| i.op == Op::CallMethodValue));
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
    fn script_top_level_new_target_is_syntax_error() {
        let err = compile_script_src_err("new.target;");
        let CompileError::Syntax { messages, .. } = err else {
            panic!("expected syntax error");
        };
        assert!(messages.iter().any(|m| m.contains("new.target")));
    }

    #[test]
    fn script_arrow_containing_new_target_is_syntax_error() {
        let err = compile_script_src_err("() => { new.target; };");
        let CompileError::Syntax { messages, .. } = err else {
            panic!("expected syntax error");
        };
        assert!(messages.iter().any(|m| m.contains("new.target")));
    }

    #[test]
    fn script_nested_function_new_target_stays_allowed() {
        let module = compile_script_src("function f() { return new.target; }");
        assert!(module.functions.iter().any(|f| f.name == "f"));
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
    fn fn_call_uses_method_value_dispatch() {
        let module = compile_script_src("function f() { return this; } f.call({});");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallMethodValue),
            "expected CallMethodValue: {:?}",
            main.code
        );
    }

    #[test]
    fn fn_apply_with_array_literal_uses_method_value_dispatch() {
        let module = compile_script_src("function f(a, b) { return a + b; } f.apply({}, [1, 2]);");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallMethodValue),
            "apply with literal array should lower to CallMethodValue: {:?}",
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
    fn fn_bind_uses_method_value_dispatch() {
        let module = compile_script_src("function f() {} f.bind({}, 1, 2);");
        let main = module.main();
        assert!(
            main.code.iter().any(|i| i.op == Op::CallMethodValue),
            "expected CallMethodValue: {:?}",
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
        // §13.15.2 NamedEvaluation — `const f = () => …` infers the
        // binding's name onto the otherwise-anonymous arrow.
        assert_eq!(arrow_fn.name, "f");
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
        // Assignment to a captured binding goes through the TDZ-checked
        // upvalue store (§6.2.4.6 PutValue).
        assert!(
            inner.code.iter().any(|i| i.op == Op::StoreUpvalueChecked),
            "inner should StoreUpvalueChecked: {:?}",
            inner.code
        );
    }

    #[test]
    fn recursive_function_declaration_captures_hoisted_binding() {
        // §10.2.11 — a function *declaration* has no funcEnv self-name
        // binding: `f` inside `f`'s body resolves to the enclosing
        // hoisted binding via an upvalue capture. The body must NOT
        // re-make its own closure (that would break identity —
        // `f !== f` — and hide expando properties).
        let module = compile_script_src(
            "function outer() { let prefix = 'x'; function f(n) { if (n <= 0) return prefix; return f(n - 1); } return f(1); }\nouter();",
        );
        let recursive = module
            .functions
            .iter()
            .find(|f| f.name == "f")
            .expect("recursive function record");
        assert!(
            !recursive
                .code
                .iter()
                .any(|i| matches!(i.op, Op::MakeClosure | Op::MakeFunction)),
            "declaration body must not re-make its own closure: {:?}",
            recursive.code
        );
        assert!(
            recursive.code.iter().any(|i| i.op == Op::LoadUpvalue),
            "self-call should load the hoisted binding through an upvalue: {:?}",
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
        // completion init + statement value + completion store + return.
        assert_eq!(main.code.len(), 4);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::LoadUndefined);
        assert_eq!(main.code[2].op, Op::StoreLocal);
        assert_eq!(main.code[3].op, Op::Return);
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
        assert_eq!(main.code.len(), 4);
    }

    #[test]
    fn interface_is_erased() {
        let module = compile_script_src("interface I { x: number; } undefined;");
        assert_eq!(module.main().code.len(), 4);
    }

    #[test]
    fn declare_function_is_erased() {
        let module = compile_script_src("declare function foo(): void; undefined;");
        assert_eq!(module.main().code.len(), 4);
    }

    #[test]
    fn import_type_is_erased() {
        let module = compile_script_src("import type { Foo } from \"./foo\"; undefined;");
        assert_eq!(module.main().code.len(), 4);
    }

    #[test]
    fn as_expression_unwraps_to_undefined() {
        let module = compile_script_src("(undefined as any);");
        // `(undefined as any)` is statement-level; LoadUndefined + Return.
        assert_eq!(module.main().code.len(), 4);
    }

    #[test]
    fn satisfies_expression_unwraps_to_undefined() {
        let module = compile_script_src("(undefined satisfies unknown);");
        assert_eq!(module.main().code.len(), 4);
    }

    #[test]
    fn non_null_unwraps_to_undefined() {
        let module = compile_script_src("undefined!;");
        assert_eq!(module.main().code.len(), 4);
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
        assert_eq!(module.main().code.len(), 4);
    }

    #[test]
    fn string_literal_compiles_to_load_string() {
        // Parenthesize to keep OXC from treating the bare literal
        // as a directive prologue.
        let module = compile_script_src("(\"abc\");");
        let main = module.main();
        // completion init + statement value + completion store + return.
        assert_eq!(main.code.len(), 4);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::LoadString);
        assert_eq!(main.code[2].op, Op::StoreLocal);
        assert_eq!(main.code[3].op, Op::Return);
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
