//! AST-to-bytecode lowering for the Ignition-style ISA.
//!
//! [`ModuleCompiler`] is the single entry point the rest of the VM uses
//! to turn a JavaScript/TypeScript source string into a
//! [`crate::module::Module`]. It owns the oxc `Allocator` for the
//! current compilation and drives the staged lowering: parse → AST
//! shape check → bytecode emit → `Module`.
//!
//! # Current state (M9)
//!
//! The compiler accepts one or more top-level `FunctionDeclaration`s
//! and lowers a narrow slice of each body. Supported surface:
//!
//! - Program is one or more `FunctionDeclaration`s. The **last**
//!   declaration becomes `Module::entry` (conventional `main` at the
//!   bottom). Functions can call each other in any order — names are
//!   collected before any body is lowered, so forward references
//!   work like JS function-declaration hoisting.
//! - Function: named (Identifier), not async, not a generator, 0 or 1
//!   parameters. The parameter must be a plain identifier — no
//!   destructuring, no default, no rest, no type annotation.
//! - Body: a `BlockStatement` whose last statement is a
//!   `ReturnStatement`. Earlier statements may be any mix of
//!   `let`/`const` declarations (top-level only — no block scoping at
//!   M7), assignment statements (`x = …;`, `x += …;`, …), `if` /
//!   `if`-`else` statements, `while` loops, nested `BlockStatement`s,
//!   and inline `return` statements (e.g. early returns inside a
//!   branch). The trailing `return` is required even when every
//!   reachable path already returns — reachability analysis lands
//!   later.
//! - `let`/`const` accept multiple declarators in one statement
//!   (`let s = 0, i = 0;`), each with its own slot allocation.
//! - Inside an `if` branch or a `while` body: assignment
//!   statements, nested control-flow statements, declarations inside
//!   block statements, and inline `return` statements are accepted.
//! - Assignment: `AssignmentExpression` whose target is a plain
//!   identifier referencing an in-scope `let`. Supported operators are
//!   `=`, `+=`, `-=`, `*=`, `|=`. Assignment to a `const`, to a
//!   parameter, or to a member/destructuring target is rejected. The
//!   accumulator is left holding the assigned value so nested
//!   assignments (`let y = x = 5;`) compose naturally.
//! - Return expression: one of
//!   - `Identifier` (parameter or in-scope `let`/`const`);
//!   - int32-safe `NumericLiteral` (integral, in `i32` range);
//!   - `BinaryExpression` with one of the int32 binary operators
//!     `+`, `-`, `*`, `|`, `&`, `^`, `<<`, `>>`, `>>>`, where each
//!     operand is itself int32-safe (identifier or int32-safe literal).
//!     Operators with a Smi opcode in the v2 ISA (`+`, `-`, `*`, `|`,
//!     `&`, `<<`, `>>`) take the `*Smi imm` fast path when the RHS is
//!     an `i8`-fit literal; the bitwise XOR (`^`) and unsigned right
//!     shift (`>>>`) have no Smi opcode, so a literal RHS would need
//!     a scratch slot the M6 frame layout does not yet allocate;
//!   - `BinaryExpression` with a relational operator `<`, `>`, `<=`,
//!     `>=`, `===`, `!==`. Lowers to `TestLessThan` /
//!     `TestGreaterThan` / `TestLessThanOrEqual` /
//!     `TestGreaterThanOrEqual` / `TestEqualStrict` (with an extra
//!     `LogicalNot` for `!==`). The accumulator-RHS-must-be-a-register
//!     constraint is satisfied via operand swapping — `n < 5` lowers
//!     as `LdaSmi 5; TestGreaterThan r_n` (i.e. `5 > n`). Two-literal
//!     comparisons (`5 < 10`) reject because neither side reaches a
//!     register without a scratch slot.
//!   - `AssignmentExpression` (so `return x = 5;` works the same as
//!     the statement form).
//!   - `CallExpression` whose callee is the name of a top-level
//!     `FunctionDeclaration` in the same module. Args are
//!     materialized into a contiguous user-visible register window
//!     allocated via [`LoweringContext::acquire_temps`] (and freed
//!     on call return); the call lowers as `CallDirect(func_idx,
//!     RegList { base, count })`. `f();` is also accepted as an
//!     `ExpressionStatement` — the result lands in the accumulator
//!     and is overwritten by the next statement.
//!
//! ## TDZ at M4
//!
//! M4 enforces the temporal dead zone **at compile time**: a `let`/
//! `const` binding becomes readable only after its own initializer is
//! lowered. Reading the binding inside its own initializer (`let x =
//! x + 1`) surfaces as `Unsupported { construct: "tdz_self_reference" }`
//! rather than executing and producing a runtime ReferenceError. This
//! is sufficient because M4 has no `AssignmentExpression` (M5), no
//! control flow (M6+), and no closures (M10+) — all the cases where
//! the compiler can't statically prove "the binding has been
//! initialized by the time we read it" land in later milestones, at
//! which point the lowering can switch to V8's pattern of
//! `LdaTheHole; Star r_x` at scope entry plus `AssertNotHole` after
//! every read.
//!
//! Anything outside that shape surfaces as a
//! [`SourceLoweringError::Unsupported`] with a `construct: &'static
//! str` tag pointing at the offending node. Unsupported is the
//! **expected** result for every milestone gap during the staged
//! rollout (see `V2_MIGRATION.md`), not a bug.
//!
//! The bytecode shape is fixed:
//!
//! ```text
//!   <return-expr lowering>   // leaves the value in the accumulator
//!   Return                    // acc is the callee's return value
//! ```
//!
//! For `function f(n) { return n + 1 }` this is:
//!
//! ```text
//!   Ldar r0      ; acc = n
//!   AddSmi 1     ; acc = n + 1
//!   Return
//! ```

mod error;
mod for_in_of;
mod optional_calls;
mod switch_scope;
mod try_finally;
mod using_decl;

#[cfg(test)]
mod tests;

pub use error::SourceLoweringError;

use std::cell::{Cell, RefCell};

use for_in_of::{
    ForInOfAssignmentTarget, ForInOfLeft, classify_for_in_of_left,
    lower_for_in_of_assignment_target,
};
use optional_calls::lower_optional_call;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrayExpression, ArrayExpressionElement, ArrayPattern, ArrowFunctionExpression,
    AssignmentExpression, AssignmentOperator, AssignmentTarget, BinaryExpression, BinaryOperator,
    BindingPattern, Class, ClassElement, ComputedMemberExpression, ConditionalExpression,
    Declaration, ExportDefaultDeclarationKind, Expression, FormalParameters, Function,
    FunctionBody, IdentifierReference, LogicalExpression, LogicalOperator, MethodDefinitionKind,
    ModuleExportName, NewExpression, NumericLiteral, ObjectExpression, ObjectPattern,
    ObjectPropertyKind, Program, PropertyKey, PropertyKind, SimpleAssignmentTarget, Statement,
    StaticMemberExpression, TemplateLiteral, UnaryExpression, UnaryOperator, UpdateExpression,
    UpdateOperator, VariableDeclaration, VariableDeclarationKind, VariableDeclarator,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use switch_scope::{enter_switch_lexical_scope, lower_switch_case_statement};
use try_finally::{
    lower_break_statement, lower_continue_statement, lower_return_statement, lower_try_statement,
};
use using_decl::{
    lower_classic_for_using_statement, lower_function_top_statement_list,
    lower_loop_using_iteration, lower_nested_statement_list, lower_top_level_statement_list,
};

use crate::bytecode::{Bytecode, BytecodeBuilder, FeedbackSlot, Label, Opcode, Operand};
use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
use crate::frame::{FrameLayout, RegisterIndex};
use crate::module::{
    ExportRecord, Function as VmFunction, FunctionIndex, FunctionTables, ImportBinding,
    ImportRecord, Module,
};

/// Staged AST-to-bytecode compiler for a single source file.
///
/// Construct one `ModuleCompiler` per source file. The compiler walks
/// the parsed AST and, when a construct is recognised, emits the
/// corresponding Ignition bytecode; unrecognised constructs produce a
/// [`SourceLoweringError::Unsupported`].
#[derive(Debug, Default)]
pub struct ModuleCompiler;

impl ModuleCompiler {
    /// Creates a new, empty compiler.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Parse and lower `source` into a [`Module`].
    ///
    /// `source_url` is used for diagnostics only — it is not fetched or
    /// resolved. `source_type` controls whether the parser treats the
    /// input as a script, module, or `.ts`/`.tsx` file; the value is
    /// forwarded verbatim to `oxc_parser`.
    ///
    /// # Errors
    ///
    /// - [`SourceLoweringError::Parse`] on parse-phase syntax errors.
    /// - [`SourceLoweringError::Unsupported`] when the AST falls outside
    ///   the currently supported M1 slice.
    pub fn compile(
        &self,
        source: &str,
        source_url: &str,
        source_type: SourceType,
    ) -> Result<Module, SourceLoweringError> {
        let _ = source_url;
        let allocator = Allocator::default();
        let parser_return = Parser::new(&allocator, source, source_type).parse();

        if !parser_return.errors.is_empty() {
            let diag = &parser_return.errors[0];
            let label_span = diag
                .labels
                .as_ref()
                .and_then(|labels| labels.first())
                .map(|label| {
                    let start = u32::try_from(label.offset()).unwrap_or(0);
                    let length = u32::try_from(label.len()).unwrap_or(0);
                    Span::new(start, start.saturating_add(length))
                })
                .unwrap_or_else(|| Span::new(0, 0));
            return Err(SourceLoweringError::Parse {
                message: diag.message.to_string(),
                span: label_span,
            });
        }

        // D2: build a source-text index once and stash it on a
        // thread-local so every nested lowering context picks it
        // up without dragging another parameter through every
        // `with_parent` call. Cleared at the end so subsequent
        // compiles (or unrelated contexts that construct a
        // `LoweringContext` directly) see `None`.
        let source_index = std::rc::Rc::new(crate::source_map::SourceTextIndex::new(source));
        SOURCE_INDEX_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(std::rc::Rc::clone(&source_index));
        });
        let result = lower_program(&parser_return.program);
        SOURCE_INDEX_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = None;
        });
        result
    }
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

fn lower_program(program: &Program<'_>) -> Result<Module, SourceLoweringError> {
    // The program is one or more top-level `FunctionDeclaration`s,
    // optionally mixed with `import` / `export` declarations (M35).
    // Anything else — `class`, `var`, top-level expressions or
    // statements — surfaces as an `Unsupported` pointing at the
    // offending node so later milestones can widen coverage one
    // construct at a time. The conventional `main` pattern
    // (helpers first, entry last) makes the **last** function the
    // module's entry for script-style programs.
    if program.body.is_empty() {
        return Err(SourceLoweringError::unsupported("program", program.span));
    }

    // M35 state: collected import/export records, plus the name of
    // every binding that the runtime installs on the global object
    // before / during module evaluation. Inner function bodies
    // resolve bare identifier references against `module_globals`
    // (via `ctx.is_module_global`) so an imported symbol or a
    // top-level export can be read/called from a nested function.
    let mut imports: Vec<ImportRecord> = Vec::new();
    let mut exports: Vec<ExportRecord> = Vec::new();
    let mut module_globals: Vec<String> = Vec::new();
    // Per-source-URL flag: this program uses ES-module syntax
    // (static `import` / `export` / dynamic `import()`). An empty
    // set of records with no `import()` expressions means the
    // program is still a plain script and lands on `Module::new`
    // (no synthesised module-init, no `new_esm`).
    let mut is_esm = false;

    // First pass: classify top-level statements into function
    // declarations (with or without an `export` wrapper), pure
    // import/export metadata, and everything else — the latter
    // makes up the "script body" that runs top-to-bottom when the
    // module is evaluated. The script-body path is the idiomatic
    // JS shape (`console.log("hi")` at file top, `const x = …`
    // followed by `fetch(...)`, etc.); no `function main() {}`
    // wrapper required.
    let mut declarations: Vec<&Function<'_>> = Vec::with_capacity(program.body.len());
    let mut names: Vec<&str> = Vec::with_capacity(program.body.len());
    let mut script_body: Vec<&Statement<'_>> = Vec::new();
    // Binding names introduced at the top level via
    // `export const` / `export let` / `export class`. The synth
    // top-level body runs their initialisers as ordinary locals;
    // the flush-to-globals loop after the body copies each local
    // onto the global object so `capture_exports` finds the value.
    let mut exported_const_vars: Vec<String> = Vec::new();
    let mut default_export_local: Option<String> = None;
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                let name = record_function_declaration(func, &mut declarations, &mut names)?;
                // Top-level function declarations are visible to
                // every top-level statement in the same module —
                // mirror them onto the global object so
                // `LdaGlobal <name>` resolves. The synth
                // top-level's CreateClosure preamble takes care
                // of the actual installation.
                if !module_globals.iter().any(|n| n == name) {
                    module_globals.push(name.to_string());
                }
            }
            Statement::ImportDeclaration(decl) => {
                is_esm = true;
                let specifier: Box<str> = decl.source.value.as_str().into();
                let mut bindings: Vec<ImportBinding> = Vec::new();
                if let Some(specs) = decl.specifiers.as_ref() {
                    for spec in specs {
                        match spec {
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                let imported = module_export_name_to_string(&s.imported)
                                    .ok_or_else(|| {
                                        SourceLoweringError::unsupported(
                                            "import_specifier_string_literal",
                                            s.span,
                                        )
                                    })?;
                                let local = s.local.name.as_str().to_string();
                                module_globals.push(local.clone());
                                bindings.push(ImportBinding::Named {
                                    imported: imported.into(),
                                    local: local.into(),
                                });
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                let local = s.local.name.as_str().to_string();
                                module_globals.push(local.clone());
                                bindings.push(ImportBinding::Default {
                                    local: local.into(),
                                });
                            }
                            oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(
                                s,
                            ) => {
                                let local = s.local.name.as_str().to_string();
                                module_globals.push(local.clone());
                                bindings.push(ImportBinding::Namespace {
                                    local: local.into(),
                                });
                            }
                        }
                    }
                }
                imports.push(ImportRecord {
                    specifier,
                    bindings,
                });
            }
            Statement::ExportNamedDeclaration(decl) => {
                is_esm = true;
                if let Some(inner) = &decl.declaration {
                    match inner {
                        Declaration::FunctionDeclaration(func) => {
                            let name = record_function_declaration(
                                func.as_ref(),
                                &mut declarations,
                                &mut names,
                            )?;
                            module_globals.push(name.to_string());
                            exports.push(ExportRecord::Named {
                                local: name.to_string().into(),
                                exported: name.to_string().into(),
                            });
                        }
                        Declaration::VariableDeclaration(var_decl) => {
                            // `export const X = expr` / `export let Y = expr`.
                            // Inject the inner VariableDeclaration into
                            // the script body so the RHS evaluates at
                            // module-eval time, then record each
                            // declarator's name so the synth top-level
                            // flushes its local to a same-named global
                            // before it returns.
                            //
                            // §16.2.3.7 Destructuring in an exported
                            // variable declaration (`export const { a,
                            // b } = obj`, `export const [x, y] =
                            // pair`) binds each leaf as its own export
                            // under its own name. Walk the pattern
                            // and collect every identifier leaf so
                            // each leaf local flushes to a same-named
                            // global.
                            for declarator in var_decl.declarations.iter() {
                                let mut leaf_names: Vec<String> = Vec::new();
                                collect_pattern_identifier_names(&declarator.id, &mut leaf_names)?;
                                for name in leaf_names {
                                    module_globals.push(name.clone());
                                    exports.push(ExportRecord::Named {
                                        local: name.clone().into(),
                                        exported: name.clone().into(),
                                    });
                                    exported_const_vars.push(name);
                                }
                            }
                            script_body.push(stmt);
                        }
                        Declaration::ClassDeclaration(_) => {
                            // `export class C {}` — route the class
                            // through the script body; top-level class
                            // declarations already lower to a local
                            // under the script-body path.
                            let name = match inner {
                                Declaration::ClassDeclaration(cls) => cls
                                    .id
                                    .as_ref()
                                    .map(|id| id.name.as_str().to_string())
                                    .ok_or_else(|| {
                                        SourceLoweringError::unsupported(
                                            "anonymous_class",
                                            inner.span(),
                                        )
                                    })?,
                                _ => unreachable!(),
                            };
                            module_globals.push(name.clone());
                            exports.push(ExportRecord::Named {
                                local: name.clone().into(),
                                exported: name.clone().into(),
                            });
                            exported_const_vars.push(name);
                            script_body.push(stmt);
                        }
                        _ => {
                            return Err(SourceLoweringError::unsupported(
                                "export_declaration_non_function",
                                inner.span(),
                            ));
                        }
                    }
                } else if let Some(source) = &decl.source {
                    // `export { x } from "./m"` — re-export named.
                    let specifier = source.value.as_str().to_string();
                    for spec in &decl.specifiers {
                        let local = module_export_name_to_string(&spec.local).ok_or_else(|| {
                            SourceLoweringError::unsupported(
                                "export_specifier_string_literal",
                                spec.span,
                            )
                        })?;
                        let exported =
                            module_export_name_to_string(&spec.exported).ok_or_else(|| {
                                SourceLoweringError::unsupported(
                                    "export_specifier_string_literal",
                                    spec.span,
                                )
                            })?;
                        exports.push(ExportRecord::ReExportNamed {
                            specifier: specifier.clone().into(),
                            imported: local.into(),
                            exported: exported.into(),
                        });
                    }
                } else {
                    // `export { x, y }` — references to top-level
                    // bindings. We record them and rely on the
                    // module-init to install the local as a global
                    // before `capture_exports` runs.
                    for spec in &decl.specifiers {
                        let local = module_export_name_to_string(&spec.local).ok_or_else(|| {
                            SourceLoweringError::unsupported(
                                "export_specifier_string_literal",
                                spec.span,
                            )
                        })?;
                        let exported =
                            module_export_name_to_string(&spec.exported).ok_or_else(|| {
                                SourceLoweringError::unsupported(
                                    "export_specifier_string_literal",
                                    spec.span,
                                )
                            })?;
                        module_globals.push(local.clone());
                        exports.push(ExportRecord::Named {
                            local: local.into(),
                            exported: exported.into(),
                        });
                    }
                }
            }
            Statement::ExportDefaultDeclaration(decl) => {
                is_esm = true;
                // §16.2.3 `export default …` — accepted shapes:
                //
                //   `export default function foo() {}` — register as
                //   a named hoistable declaration.
                //
                //   `export default class Foo {}` — hoist onto the
                //   synthesised top-level script via the same path
                //   as a plain top-level class declaration; bind the
                //   default export to `Foo` on the global object.
                //
                //   Anonymous defaults (`export default class {}` /
                //   `export default function () {}` / `export
                //   default expr`) synthesise a fresh module-level
                //   binding and lower at module-init time.
                match &decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(func)
                        if func.id.is_some() =>
                    {
                        let name = record_function_declaration(
                            func.as_ref(),
                            &mut declarations,
                            &mut names,
                        )?;
                        default_export_local = Some(name.to_string());
                        module_globals.push(name.to_string());
                        exports.push(ExportRecord::Default {
                            local: name.to_string().into(),
                        });
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(class) if class.id.is_some() => {
                        let id = class.id.as_ref().expect("guard ensures named class");
                        let name = id.name.as_str().to_string();
                        // Route through the statement-lowering phase
                        // like any other top-level class. The outer
                        // `Statement::ExportDefaultDeclaration` itself
                        // drops into `script_body` unchanged; the
                        // script-body lowerer recognises the default
                        // wrapper and delegates to
                        // `lower_nested_class_declaration`.
                        module_globals.push(name.clone());
                        exported_const_vars.push(name.clone());
                        default_export_local = Some(name.clone());
                        exports.push(ExportRecord::Default { local: name.into() });
                        script_body.push(stmt);
                    }
                    other => {
                        // `export default <expr>` and anonymous
                        // `export default function () {}` / `export
                        // default class {}` all collapse to
                        // "evaluate the right-hand side at module
                        // init, bind to a synthetic module-level
                        // `default` local, and register that local
                        // as the default export." The binding is
                        // named `__otter_default` — reserved and
                        // not reachable by user identifier refs
                        // (starts with `__otter_` which the compiler
                        // treats as internal).
                        if !matches!(
                            other,
                            ExportDefaultDeclarationKind::ClassDeclaration(_)
                                | ExportDefaultDeclarationKind::FunctionDeclaration(_)
                        ) && !other.is_expression()
                        {
                            return Err(SourceLoweringError::unsupported(
                                "export_default_non_function",
                                decl.span,
                            ));
                        }
                        module_globals.push(MODULE_DEFAULT_EXPORT_LOCAL.to_string());
                        exported_const_vars.push(MODULE_DEFAULT_EXPORT_LOCAL.to_string());
                        default_export_local = Some(MODULE_DEFAULT_EXPORT_LOCAL.to_string());
                        exports.push(ExportRecord::Default {
                            local: MODULE_DEFAULT_EXPORT_LOCAL.into(),
                        });
                        script_body.push(stmt);
                    }
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                is_esm = true;
                let specifier: Box<str> = decl.source.value.as_str().into();
                if let Some(exported) = &decl.exported {
                    let exported = module_export_name_to_string(exported).ok_or_else(|| {
                        SourceLoweringError::unsupported(
                            "export_specifier_string_literal",
                            decl.span,
                        )
                    })?;
                    exports.push(ExportRecord::ReExportNamespace {
                        specifier,
                        exported: exported.into(),
                    });
                } else {
                    exports.push(ExportRecord::ReExportAll { specifier });
                }
            }
            Statement::ClassDeclaration(class) => {
                // Top-level class declarations are visible to every
                // other top-level function in the module — add the
                // name to `module_globals` + `exported_const_vars`
                // so the synth top-level flushes the local to a
                // global of the same name. Inner methods of a
                // top-level function can then refer to the class
                // via `LdaGlobal`.
                if let Some(id) = &class.id {
                    let name = id.name.as_str().to_string();
                    module_globals.push(name.clone());
                    exported_const_vars.push(name);
                }
                script_body.push(stmt);
            }
            Statement::VariableDeclaration(decl) => {
                // §14.2 top-level `let` / `const` / `var` bindings
                // in a module are lexically scoped to the module —
                // but they're visible to every top-level function
                // in the same file (closures over module scope).
                // We don't have a closure from top-level functions
                // into the synth body, so mirror those names onto
                // the global object instead. `function circleArea
                // () { return PI * r * r }` after `const PI = …`
                // resolves `PI` via `LdaGlobal` at call time.
                for declarator in decl.declarations.iter() {
                    if let oxc_ast::ast::BindingPattern::BindingIdentifier(bi) = &declarator.id {
                        let name = bi.name.as_str().to_string();
                        module_globals.push(name.clone());
                        exported_const_vars.push(name);
                    }
                }
                script_body.push(stmt);
            }
            other => {
                // Top-level script statement — `console.log(...)`,
                // `if (...) { ... }`, etc. Collect into
                // `script_body` and synthesise a top-level entry
                // function that runs them on module evaluation.
                script_body.push(other);
            }
        }
    }

    let _ = default_export_local;

    // A module is valid when it has at least one source-level
    // artefact: a function, a top-level statement, or an
    // import/export record. Empty source (all whitespace) still
    // gets rejected.
    if declarations.is_empty() && script_body.is_empty() && !is_esm {
        return Err(SourceLoweringError::unsupported("program", program.span));
    }

    // Second pass: lower each function with the shared name table
    // available so `f(args)` inside one body can resolve `f` to its
    // `FunctionIndex`. Top-level functions land at indices
    // `0..declarations.len()`; any inner `FunctionExpression`
    // encountered during body lowering appends beyond that.
    let module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>> = std::rc::Rc::new(
        std::cell::RefCell::new(Vec::with_capacity(declarations.len())),
    );
    // M25: top-level declaration indices need to be stable before
    // any body lowering runs (so nested `f()` inside one body can
    // resolve to the shared `function_names` table). We push
    // placeholder `VmFunction::empty` entries and then overwrite
    // each slot with the real lowered function. Inner functions
    // (landing after the top-level slots) use `Vec::push` to grow
    // the shared list.
    for _ in 0..declarations.len() {
        module_functions.borrow_mut().push(placeholder_function());
    }

    // M35: publish `module_globals` via a shared top-level
    // `LoweringContext` before any body is lowered. Every child
    // context inherits the populated list via the `Rc`, so nested
    // function bodies that reference an imported symbol resolve
    // it through `LdaGlobal` without knowing about module
    // machinery. The context lives only long enough to seed the
    // list — each `lower_function_declaration` creates its own
    // context internally (with no parent), so it picks up the
    // names by constructing a fresh `Rc`. To share, we clone the
    // Rc into every call.
    let module_globals_rc: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(module_globals.clone()));

    for (top_idx, func) in declarations.iter().enumerate() {
        let lowered = lower_function_declaration_with_globals(
            func,
            &names,
            std::rc::Rc::clone(&module_functions),
            std::rc::Rc::clone(&module_globals_rc),
        )?;
        module_functions.borrow_mut()[top_idx] = lowered;
    }

    // Entry: always the synthesised top-level function. The ES
    // spec has no notion of a "main" function — a module / script
    // is just the statements at the top level, evaluated once when
    // the module loads. Top-level function declarations stay
    // callable via their `FunctionIndex` / `CallDirect`, but
    // nothing auto-invokes them; explicit calls (top-level or
    // inside another function) are the only entry points, matching
    // real JS semantics.
    //
    // For ES modules the synth's preamble installs each exported
    // top-level binding on the global object so `capture_exports`
    // in the module loader sees the values.
    //
    // Classic scripts that declare only functions (no imperative
    // statements) still get a top-level entry — its body is just
    // the trailing `LdaUndefined; Return` pair, which runs once
    // and exits with no observable side effect.
    let top_idx = synthesise_top_level_entry(
        &module_functions,
        &names,
        &module_globals,
        &script_body,
        &exported_const_vars,
    )?;
    let entry_idx = u32::try_from(top_idx)
        .map_err(|_| SourceLoweringError::Internal("top-level entry index overflow".into()))?;

    let functions = std::rc::Rc::try_unwrap(module_functions)
        .map_err(|_| SourceLoweringError::Internal("module functions still shared".into()))?
        .into_inner();
    let module = if is_esm {
        Module::new_esm(
            None::<&str>,
            functions,
            FunctionIndex(entry_idx),
            imports,
            exports,
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("module construction failed: {err}"))
        })?
    } else {
        Module::new(None::<&str>, functions, FunctionIndex(entry_idx)).map_err(|err| {
            SourceLoweringError::Internal(format!("module construction failed: {err}"))
        })?
    };
    Ok(module)
}

/// Records a single `FunctionDeclaration` into the module's
/// top-level declaration tables. Rejects anonymous or duplicate
/// names with a stable tag so callers don't repeat the check.
fn record_function_declaration<'a>(
    func: &'a Function<'a>,
    declarations: &mut Vec<&'a Function<'a>>,
    names: &mut Vec<&'a str>,
) -> Result<&'a str, SourceLoweringError> {
    let name = func
        .id
        .as_ref()
        .map(|ident| ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;
    if names.contains(&name) {
        return Err(SourceLoweringError::unsupported(
            "duplicate_function_declaration",
            func.span,
        ));
    }
    names.push(name);
    declarations.push(func);
    Ok(name)
}

/// Recursively walks a `BindingPattern` and pushes every
/// `BindingIdentifier` leaf's name onto `out`. Used by
/// `export const { a, b } = obj` / `export const [x, y] = pair`
/// to collect every export-generating leaf name. Rest elements
/// (`export const [...rest] = arr`, `export const { ...rest } =
/// obj`) also bind a name and are included. Default initializers
/// on a leaf (`export const { a = 1 } = obj`) peel back to the
/// BindingIdentifier via the AssignmentPattern wrapper.
fn collect_pattern_identifier_names<'a>(
    pattern: &'a oxc_ast::ast::BindingPattern<'a>,
    out: &mut Vec<String>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::BindingPattern;
    match pattern {
        BindingPattern::BindingIdentifier(ident) => {
            out.push(ident.name.as_str().to_string());
            Ok(())
        }
        BindingPattern::ArrayPattern(pat) => {
            for element in pat.elements.iter().flatten() {
                collect_pattern_identifier_names(element, out)?;
            }
            if let Some(rest) = pat.rest.as_deref() {
                collect_pattern_identifier_names(&rest.argument, out)?;
            }
            Ok(())
        }
        BindingPattern::ObjectPattern(pat) => {
            for prop in &pat.properties {
                collect_pattern_identifier_names(&prop.value, out)?;
            }
            if let Some(rest) = pat.rest.as_deref() {
                collect_pattern_identifier_names(&rest.argument, out)?;
            }
            Ok(())
        }
        BindingPattern::AssignmentPattern(pat) => {
            // `{ a = 1 }` / `[a = 1]` — the left side is the
            // actual binding; the right is the default.
            collect_pattern_identifier_names(&pat.left, out)
        }
    }
}

/// §16.2.1.4 — converts a `ModuleExportName` AST node (which may be
/// an identifier or a string literal) into the bare string form
/// the runtime records use. Returns `None` for string-literal
/// names because the current module surface doesn't carry those
/// through the runtime registry yet (`"foo \0 bar"` export names
/// need additional care around UTF-16 and property-key interning).
fn module_export_name_to_string(name: &ModuleExportName<'_>) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(i) => Some(i.name.as_str().to_string()),
        ModuleExportName::IdentifierReference(i) => Some(i.name.as_str().to_string()),
        ModuleExportName::StringLiteral(_) => None,
    }
}

const MODULE_DEFAULT_EXPORT_LOCAL: &str = "__otter_default";

/// Appends a synthetic "module-init" [`VmFunction`] to
/// `module_functions`. Its body materialises each top-level
/// declaration whose name is a module global as a closure on the
/// global object so the module loader's `capture_exports` can
/// read the value back out under that name. Returns the appended
/// index.
/// Builds the module's top-level entry function from the
/// collected script-body statements. Runs them top-to-bottom
/// with full local / temp / closure support — the same body
/// lowering every regular function uses. For ES modules, the
/// preamble also installs each exported top-level binding on
/// the global object so `capture_exports` in the module loader
/// sees the values by the time it harvests the namespace
/// (same contract as `synthesise_module_init_function`,
/// delivered inline here instead of in a separate function).
fn synthesise_top_level_entry<'a>(
    module_functions: &std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    names: &[&str],
    module_globals: &[String],
    script_body: &[&'a Statement<'a>],
    exported_const_vars: &[String],
) -> Result<usize, SourceLoweringError> {
    // Empty params — the top-level entry takes no arguments.
    // `names` carries the top-level function-declaration names so
    // `f()` inside the script body can still resolve to its
    // `FunctionIndex` and emit `CallDirect`.
    let params_layout = ParamsLayout {
        names: Vec::new(),
        defaults: Vec::new(),
        patterns: Vec::new(),
        rest_name: None,
        rest_pattern: None,
    };
    let mut builder = BytecodeBuilder::new();
    // Publish `module_globals` via the thread-local override so
    // the newly-built `LoweringContext` picks up the full list —
    // the preamble and script body both need to know which
    // top-level names the module considers module-global, so
    // `lower_identifier_reference` routes bare references via
    // `LdaGlobal` (same channel the user-declared top-level
    // functions already use).
    let globals_rc: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(module_globals.to_vec()));
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(std::rc::Rc::clone(&globals_rc));
    });
    let mut ctx = LoweringContext::new(&params_layout, names, std::rc::Rc::clone(module_functions));
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = None;
    });

    // Preamble: install each module-global binding on the global
    // object so ESM `capture_exports` finds them. For classic
    // scripts `module_globals` is empty and this loop is a
    // no-op — top-level `let` / `const` still uses the regular
    // local-allocation path in the body lowering.
    let mut pending_templates: Vec<(u32, crate::closure::ClosureTemplate)> = Vec::new();
    for name in module_globals {
        let Some(top_idx) = names.iter().position(|n| *n == name.as_str()) else {
            continue;
        };
        let func_idx = u32::try_from(top_idx).map_err(|_| {
            SourceLoweringError::Internal("top-level function index overflow".into())
        })?;
        let pc = builder
            .emit(
                Opcode::CreateClosure,
                &[Operand::Idx(func_idx), Operand::Imm(0)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("top-level encode CreateClosure: {err:?}"))
            })?;
        pending_templates.push((
            pc,
            crate::closure::ClosureTemplate::new(FunctionIndex(func_idx), Vec::new()),
        ));
        let prop_idx = ctx.intern_property_name(name)?;
        builder
            .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("top-level encode StaGlobal: {err:?}"))
            })?;
    }

    // Main body: lower each collected top-level statement through
    // the same path function bodies use.
    lower_top_level_statement_list(&mut builder, &mut ctx, script_body)?;
    // Post-body flush: `export const X = expr` allocated a local
    // for `X` during script-body lowering. Copy each local onto
    // the global object so the module-loader's `capture_exports`
    // sees the value when it walks the module namespace.
    for name in exported_const_vars {
        let Some(binding) = ctx.resolve_identifier(name) else {
            continue;
        };
        let reg = match binding {
            BindingRef::Local {
                reg,
                initialized: true,
                ..
            } => reg,
            BindingRef::Param { reg } => reg,
            _ => continue,
        };
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "top-level encode Ldar (export flush): {err:?}"
                ))
            })?;
        let prop_idx = ctx.intern_property_name(name)?;
        builder
            .emit(Opcode::StaGlobal, &[Operand::Idx(prop_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "top-level encode StaGlobal (export flush): {err:?}"
                ))
            })?;
    }
    // Explicit `LdaUndefined; Return` tail — the module's
    // evaluation completion value is always `undefined` for a
    // script-style top-level.
    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("top-level encode LdaUndefined: {err:?}"))
    })?;
    builder.emit(Opcode::Return, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("top-level encode Return: {err:?}"))
    })?;

    // Resolve pending exception handlers + bytecode length BEFORE
    // `builder.finish()` consumes the builder — `finish` drops
    // the label state that `take_exception_handlers` needs to
    // resolve try/catch PCs. A stale `BytecodeBuilder::new()`
    // would see every label as unbound and surface as
    // `exception handler try_start unbound`.
    let exception_handlers = ctx.take_exception_handlers(&builder)?;
    let bytecode_len_u32 = builder.pc();
    // Merge compiler-tracked closure templates (from nested
    // function expressions inside the script body) with our
    // prepended CreateClosure preamble entries.
    let mut closure_vec: Vec<Option<crate::closure::ClosureTemplate>> =
        vec![None; bytecode_len_u32 as usize];
    let compiler_templates = ctx.take_closure_table(bytecode_len_u32);
    for pc in 0..bytecode_len_u32 {
        if let Some(tpl) = compiler_templates.get(pc) {
            closure_vec[pc as usize] = Some(tpl);
        }
    }
    for (pc, tpl) in pending_templates {
        closure_vec[pc as usize] = Some(tpl);
    }
    let closure_table = crate::closure::ClosureTable::new(closure_vec);

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("top-level finish: {err:?}")))?;
    let layout = FrameLayout::new(1, 0, ctx.local_count(), ctx.temp_count())
        .map_err(|err| SourceLoweringError::Internal(format!("top-level layout: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&ctx.take_feedback_slot_kinds());
    let side_tables = crate::module::FunctionSideTables::new(
        ctx.take_property_names(),
        ctx.take_string_literals(),
        ctx.take_float_constants(),
        ctx.take_bigint_constants(),
        closure_table,
        Default::default(),
        ctx.take_regexp_literals(),
    );
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        crate::exception::ExceptionTable::new(exception_handlers),
        ctx.take_source_map(),
    );
    let vm_fn = VmFunction::new(Some("<top-level>"), layout, bytecode, tables);
    let mut fns = module_functions.borrow_mut();
    let idx = fns.len();
    fns.push(vm_fn);
    Ok(idx)
}

fn synthesise_module_init_function(
    module_functions: &std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    names: &[&str],
    module_globals: &[String],
) -> Result<usize, SourceLoweringError> {
    // Hidden[0] is still the (unused) receiver slot, matching the
    // conventional frame layout for every other top-level
    // function. No params, no scratch. A single `u8` of property
    // names is plenty for the immediate subset.
    let layout = FrameLayout::new(1, 0, 0, 0)
        .map_err(|e| SourceLoweringError::Internal(format!("module-init layout: {e:?}")))?;
    let mut builder = BytecodeBuilder::new();
    let mut property_names: Vec<String> = Vec::new();
    // PC → ClosureTemplate map, built alongside bytecode emission.
    // The runtime looks up the template for each `CreateClosure`
    // opcode via `ClosureTable::get(pc)`; a missing entry trips
    // the "no ClosureTemplate for this PC" native-call error, so
    // every CreateClosure here must register one.
    let mut pending_templates: Vec<(u32, crate::closure::ClosureTemplate)> = Vec::new();
    for name in module_globals {
        let Some(top_idx) = names.iter().position(|n| *n == name.as_str()) else {
            continue;
        };
        let func_idx = u32::try_from(top_idx).map_err(|_| {
            SourceLoweringError::Internal("module-init function index overflow".into())
        })?;
        let pc = builder
            .emit(
                Opcode::CreateClosure,
                &[Operand::Idx(func_idx), Operand::Imm(0)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("module-init encode CreateClosure: {err:?}"))
            })?;
        pending_templates.push((
            pc,
            crate::closure::ClosureTemplate::new(FunctionIndex(func_idx), Vec::new()),
        ));
        let prop_idx = property_names
            .iter()
            .position(|existing| existing == name)
            .unwrap_or_else(|| {
                property_names.push(name.clone());
                property_names.len() - 1
            });
        builder
            .emit(
                Opcode::StaGlobal,
                &[Operand::Idx(u32::try_from(prop_idx).unwrap_or(u32::MAX))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("module-init encode StaGlobal: {err:?}"))
            })?;
    }
    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("module-init encode LdaUndefined: {err:?}"))
    })?;
    builder.emit(Opcode::Return, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("module-init encode Return: {err:?}"))
    })?;

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("module-init finish: {err:?}")))?;
    let bytecode_len = bytecode.bytes().len();
    let mut templates: Vec<Option<crate::closure::ClosureTemplate>> = vec![None; bytecode_len];
    for (pc, template) in pending_templates {
        let idx = pc as usize;
        if idx < templates.len() {
            templates[idx] = Some(template);
        }
    }
    let closure_table = crate::closure::ClosureTable::new(templates);
    let side_tables = crate::module::FunctionSideTables::new(
        crate::property::PropertyNameTable::new(property_names),
        crate::string::StringTable::default(),
        crate::float::FloatTable::default(),
        crate::bigint::BigIntTable::default(),
        closure_table,
        crate::call::CallTable::default(),
        crate::regexp::RegExpTable::default(),
    );
    let tables = FunctionTables::new(
        side_tables,
        FeedbackTableLayout::default(),
        crate::deopt::DeoptTable::default(),
        crate::exception::ExceptionTable::default(),
        crate::source_map::SourceMap::default(),
    );
    let vm_fn = VmFunction::new(Some("<module-init>"), layout, bytecode, tables);
    let mut fns = module_functions.borrow_mut();
    let idx = fns.len();
    fns.push(vm_fn);
    Ok(idx)
}

/// Variant of [`lower_function_declaration`] that injects a shared
/// `module_globals` table into the lowering context so nested
/// function bodies resolve imported / exported names via
/// `LdaGlobal`. The plain [`lower_function_declaration`] keeps
/// its signature for backwards compatibility with internal
/// callers (nested-closure recursion).
fn lower_function_declaration_with_globals<'a>(
    func: &'a Function<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    module_globals: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
) -> Result<VmFunction, SourceLoweringError> {
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(module_globals);
    });
    let result = lower_function_declaration(func, function_names, module_functions);
    MODULE_GLOBALS_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = None;
    });
    result
}

std::thread_local! {
    /// Temporary channel carrying the module-globals table from
    /// [`lower_program`] down into the top-level
    /// [`LoweringContext::new`] call sites. `lower_function_declaration`
    /// constructs a `LoweringContext` with `parent = None` (no
    /// natural inheritance path), so a thread-local override is the
    /// least invasive way to seed the list without threading a
    /// module-state parameter through every compiler entry point.
    /// Cleared in `lower_function_declaration_with_globals` once the
    /// top-level body has been lowered — child contexts inherit the
    /// `Rc` via `with_parent`.
    static MODULE_GLOBALS_OVERRIDE: std::cell::RefCell<
        Option<std::rc::Rc<std::cell::RefCell<Vec<String>>>>,
    > = const { std::cell::RefCell::new(None) };

    /// D2: Channel carrying the current module's
    /// `SourceTextIndex` from `ModuleCompiler::compile` into
    /// every nested `LoweringContext` so opcode emission can
    /// record `(pc → (line, column))` entries without plumbing
    /// the index through every helper.
    static SOURCE_INDEX_OVERRIDE: std::cell::RefCell<
        Option<std::rc::Rc<crate::source_map::SourceTextIndex>>,
    > = const { std::cell::RefCell::new(None) };
}

/// Maps the residual `Statement` variants we explicitly don't handle at
/// M1 back to a stable `construct` tag. Later milestones can move a row
/// from this catch-all into a real lowering arm without touching call
/// sites in tests.
fn statement_construct_tag(stmt: &Statement<'_>) -> &'static str {
    match stmt {
        Statement::VariableDeclaration(_) => "variable_declaration",
        Statement::ExpressionStatement(_) => "expression_statement",
        Statement::IfStatement(_) => "if_statement",
        Statement::WhileStatement(_) => "while_statement",
        Statement::DoWhileStatement(_) => "do_while_statement",
        Statement::ForStatement(_) => "for_statement",
        Statement::BlockStatement(_) => "block_statement",
        Statement::ReturnStatement(_) => "return_statement",
        Statement::ImportDeclaration(_) | Statement::ExportAllDeclaration(_) => {
            "module_declaration"
        }
        Statement::ExportDefaultDeclaration(_) | Statement::ExportNamedDeclaration(_) => {
            "export_declaration"
        }
        _ => "statement",
    }
}

/// Placeholder `Function` used to reserve top-level module slots
/// before bodies are lowered. Each slot is overwritten with the
/// real lowered function at the end of
/// `lower_program`; any nested `FunctionExpression` pushes beyond
/// the top-level prefix without shifting reserved indices.
fn placeholder_function() -> VmFunction {
    let layout = FrameLayout::new(0, 0, 0, 0).expect("empty frame layout");
    let empty_bytecode = BytecodeBuilder::new()
        .finish()
        .expect("empty BytecodeBuilder finishes");
    VmFunction::with_empty_tables(None::<&'static str>, layout, empty_bytecode)
}

fn lower_function_declaration<'a>(
    func: &'a Function<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
) -> Result<VmFunction, SourceLoweringError> {
    let name = func
        .id
        .as_ref()
        .map(|ident| ident.name.as_str().to_owned())
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;

    let params_layout = analyze_params(&func.params)?;
    let param_count = params_layout.param_slot_count();

    let body = func
        .body
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("declared_only_function", func.span))?;

    // Lower the body first so we know the final `let`/`const`,
    // call-temp, feedback-slot counts, and the interned
    // property-name / float-constant tables (M14). FrameLayout
    // needs the first two up front, and the feedback slot count
    // seeds the function's `FeedbackTableLayout` for the JIT's
    // int32-trust consumer (see
    // `analyze_template_candidate_with_feedback`).
    let body_out = lower_function_body(
        body,
        &func.params,
        &params_layout,
        function_names,
        module_functions,
    )?;

    // FrameLayout: 1 hidden slot for `this`, then `param_count`
    // parameter slots (non-rest params only; rest lands in a local),
    // then `local_count` `let`/`const` + rest-param slots, then
    // `temp_count` call-arg scratch slots. The v2 interpreter maps
    // `Ldar r0` through `FrameLayout::resolve_user_visible(0)`, which
    // points at the first parameter (absolute index 1), so parameter
    // / local / temp access stays symmetric with v1's register
    // semantics.
    let layout = FrameLayout::new(1, param_count, body_out.local_count, body_out.temp_count)
        .map_err(|err| SourceLoweringError::Internal(format!("frame layout invalid: {err:?}")))?;

    // M_JIT_C.2: every arithmetic op emitted above allocated a fresh
    // `Arithmetic`-kind slot via `allocate_arithmetic_feedback`. Build
    // the matching side-table layout so the interpreter and JIT can
    // resolve `bytecode.feedback().get(pc) -> FeedbackSlot` against a
    // well-shaped `FeedbackVector`.
    let feedback_layout = feedback_layout_from_kinds(&body_out.feedback_slot_kinds);
    // M14 / M15 / M25: wire the accumulated side tables so the
    // dispatcher can resolve `Idx` operands at runtime
    // (property names, string literals, float constants) and
    // materialise closures at CreateClosure PCs (closure
    // templates). Other tables (bigints, calls, regexps) stay
    // default-empty until later milestones exercise them.
    let side_tables = crate::module::FunctionSideTables::new(
        body_out.property_names,
        body_out.string_literals,
        body_out.float_constants,
        body_out.bigint_constants,
        body_out.closures,
        Default::default(),
        body_out.regexp_literals,
    );
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        body_out.exceptions,
        body_out.source_map,
    );

    Ok(
        VmFunction::new(Some(name), layout, body_out.bytecode, tables)
            .with_strict(func.id.is_some())
            .with_async(func.r#async)
            .with_generator(func.generator),
    )
}

/// Output of [`lower_function_body`]. Groups the bytecode with the
/// per-function side-table counts the caller wires into the
/// `Function`.
struct FunctionBodyOutput {
    bytecode: Bytecode,
    local_count: RegisterIndex,
    temp_count: RegisterIndex,
    feedback_slot_count: u16,
    /// P1: per-slot feedback kinds, in allocation order. Used to
    /// build a heterogeneous `FeedbackTableLayout` — arithmetic
    /// feedback alongside property inline-cache feedback, call
    /// target feedback, etc.
    feedback_slot_kinds: Vec<FeedbackKind>,
    property_names: crate::property::PropertyNameTable,
    float_constants: crate::float::FloatTable,
    string_literals: crate::string::StringTable,
    bigint_constants: crate::bigint::BigIntTable,
    regexp_literals: crate::regexp::RegExpTable,
    exceptions: crate::exception::ExceptionTable,
    closures: crate::closure::ClosureTable,
    /// D2: `pc → (line, column)` map built from statement-level
    /// recordings during lowering. Empty when the compilation
    /// wasn't fed a source-text index (synthesised functions,
    /// test harnesses constructing modules manually).
    source_map: crate::source_map::SourceMap,
}

/// Build a `FeedbackTableLayout` matching the kinds observed by the
/// lowering context. Source-compiled functions allocate slots in
/// monotonically increasing order, so mapping index → (slot id, kind)
/// lines up with the slot ids produced by
/// `LoweringContext::allocate_*_feedback`.
fn feedback_layout_from_kinds(kinds: &[FeedbackKind]) -> FeedbackTableLayout {
    let slots: Vec<FeedbackSlotLayout> = kinds
        .iter()
        .enumerate()
        .map(|(i, k)| {
            FeedbackSlotLayout::new(FeedbackSlotId(u16::try_from(i).unwrap_or(u16::MAX)), *k)
        })
        .collect();
    FeedbackTableLayout::new(slots)
}

/// Structured result of `analyze_params`. Captures what the body
/// lowerer needs to emit correct parameter-setup bytecode at
/// function entry.
///
/// - `names[i]` — identifier name of the i-th non-rest parameter.
/// - `defaults[i]` — `Some(expr)` when the i-th param has a
///   default initializer; `None` otherwise.
/// - `rest_name` — `Some(name)` when the function has a rest
///   parameter (`function f(..., ...rest)`); `None` otherwise.
///
/// The rest parameter lives in a dedicated local slot (allocated
/// at body-lowering time), **not** in the parameter slot window —
/// the runtime's `CallDirect` / `CallProperty` paths copy only
/// non-rest arguments into parameter slots, with anything beyond
/// that count stashed in `activation.overflow_args` for the
/// `CreateRestParameters` opcode at function entry to pull into an
/// array.
struct ParamsLayout<'a> {
    names: Vec<&'a str>,
    defaults: Vec<Option<&'a Expression<'a>>>,
    /// Per-param destructuring pattern. `Some(&pat)` means the
    /// param occupies a slot reserved for the raw argument value,
    /// and `emit_param_destructuring` must bind the pattern's
    /// leaves to fresh locals after the default-initializer pass.
    /// `None` means the param is a plain identifier at slot `i`
    /// and `names[i]` is the user-facing binding.
    patterns: Vec<Option<&'a BindingPattern<'a>>>,
    rest_name: Option<&'a str>,
    /// `function f(...[a, b])` — destructuring rest parameter.
    /// When set, the rest array still lands in an anonymous
    /// local; `emit_rest_parameter` then runs a pattern-bind
    /// against it to populate the leaf identifiers.
    rest_pattern: Option<&'a BindingPattern<'a>>,
}

impl ParamsLayout<'_> {
    /// Count of actual parameter slots the FrameLayout reserves —
    /// one per non-rest param (the rest binding is a local, not a
    /// param slot).
    fn param_slot_count(&self) -> RegisterIndex {
        RegisterIndex::try_from(self.names.len()).unwrap_or(u16::MAX)
    }
}

/// Walks a `FormalParameters` list, validates every param shape we
/// support at M22 (plain identifier patterns, optional default
/// initializer, optional single rest parameter), and produces a
/// `ParamsLayout` the body lowerer can drive off of.
///
/// Accepted shapes (per-param):
/// - `name` — plain identifier.
/// - `name = <expr>` — identifier with default initializer.
///
/// Accepted rest shape:
/// - `...rest` — plain identifier. No default allowed on rest
///   (spec forbids it anyway).
///
/// Parser-recovery guards:
/// - `parser_recovery_formal_param_assignment` — oxc documents
///   top-level `AssignmentPattern` as invalid in `FormalParameter`;
///   real parameter defaults arrive through `param.initializer`.
/// - `parser_recovery_rest_parameter_pattern` — rest initializers
///   are syntax errors before lowering; identifier and
///   destructuring rest patterns are first-class surfaces.
fn analyze_params<'a>(
    params: &'a FormalParameters<'a>,
) -> Result<ParamsLayout<'a>, SourceLoweringError> {
    let mut names = Vec::with_capacity(params.items.len());
    let mut defaults = Vec::with_capacity(params.items.len());
    let mut patterns = Vec::with_capacity(params.items.len());

    for param in params.items.iter() {
        match &param.pattern {
            BindingPattern::BindingIdentifier(ident) => {
                names.push(ident.name.as_str());
                defaults.push(param.initializer.as_deref());
                patterns.push(None);
            }
            // M24: array / object destructuring parameter. The
            // param slot is synthesized (user code can't reach it
            // — `@p` isn't a legal JS identifier), and
            // `emit_param_destructuring` binds the pattern's
            // leaves to fresh locals after the default-init pass.
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                names.push("@p");
                defaults.push(param.initializer.as_deref());
                patterns.push(Some(&param.pattern));
            }
            // `function f(x = 5)` comes through the `BindingIdentifier`
            // path above — oxc flattens the default into
            // `param.initializer`, not into an AssignmentPattern.
            // AssignmentPattern at this level is parser recovery:
            // real defaults are carried by `param.initializer`.
            BindingPattern::AssignmentPattern(pat) => {
                return Err(SourceLoweringError::unsupported(
                    "parser_recovery_formal_param_assignment",
                    pat.span,
                ));
            }
        }
    }

    // Optional rest parameter. oxc wraps `...rest` in
    // `FormalParameters.rest: FormalParameterRest`, which itself
    // contains a `BindingRestElement { argument: BindingPattern }`.
    // Supports identifier rest (`function f(...rest)`) and
    // destructuring rest (`function f(...[a, b])` / `...{ a }`).
    let (rest_name, rest_pattern) = match params.rest.as_deref() {
        Some(rest) => match &rest.rest.argument {
            BindingPattern::BindingIdentifier(ident) => (Some(ident.name.as_str()), None),
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                (None, Some(&rest.rest.argument))
            }
            _ => {
                return Err(SourceLoweringError::unsupported(
                    "parser_recovery_rest_parameter_pattern",
                    rest.rest.span,
                ));
            }
        },
        None => (None, None),
    };

    Ok(ParamsLayout {
        names,
        defaults,
        patterns,
        rest_name,
        rest_pattern,
    })
}

/// Emits per-parameter default-initializer bytecode at function
/// entry, in declaration order. For each param with `default = Some(expr)`:
///
/// ```text
///   Ldar r_param                ; acc = caller-supplied arg (or undefined)
///   JumpIfNotUndefined skip
///   <lower default expr>         ; acc = default value
///   Star r_param
/// skip:
/// ```
///
/// Spec: §10.2.1 FunctionDeclarationInstantiation — defaults only
/// evaluate when the parameter binding is `undefined`, matching
/// both "caller omitted the argument" and "caller passed explicit
/// `undefined`".
///
/// Later defaults can reference earlier params (`f(a, b = a + 1)`)
/// because the iteration is in source order and each default
/// `Star`s into the param slot before the next default runs.
fn emit_default_initializers<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    layout: &ParamsLayout<'a>,
) -> Result<(), SourceLoweringError> {
    for (i, default) in layout.defaults.iter().enumerate() {
        let Some(expr) = default else { continue };
        let reg = u32::try_from(i)
            .map_err(|_| SourceLoweringError::Internal("param index overflow".into()))?;
        let skip = builder.new_label();
        // Ldar reads the param slot into acc. We intentionally
        // skip the feedback-slot attachment that
        // `lower_identifier_read` would add — this is a one-shot
        // prologue read, and polluting the feedback vector with
        // it would mark every default as `Any` for no gain.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(reg)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (default init): {err:?}"))
            })?;
        builder
            .emit_jump_to(Opcode::JumpIfNotUndefined, skip)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfNotUndefined (default): {err:?}"
                ))
            })?;
        // Lower default expression into acc, then spill.
        lower_return_expression(builder, ctx, expr)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(reg)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (default init): {err:?}"))
            })?;
        builder
            .bind_label(skip)
            .map_err(|err| SourceLoweringError::Internal(format!("bind default skip: {err:?}")))?;
    }
    Ok(())
}

/// For each destructuring parameter (array or object pattern),
/// emits the binding code that extracts leaves from the synthetic
/// param slot into fresh locals. Runs after
/// `emit_default_initializers` so `{ a = 1 }` per-leaf defaults
/// see the post-default param value.
///
/// Mirrors the `let` destructuring lowering — same
/// `lower_pattern_bind` helper, different "source register"
/// (the param slot, not a hidden local).
fn emit_param_destructuring<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    layout: &ParamsLayout<'a>,
) -> Result<(), SourceLoweringError> {
    for (i, pattern) in layout.patterns.iter().enumerate() {
        let Some(pat) = pattern else { continue };
        let param_reg = RegisterIndex::try_from(i)
            .map_err(|_| SourceLoweringError::Internal("param index overflow".into()))?;
        // Params are ordinary writable bindings (M22), so we pass
        // `is_const: false` — matches the spec's Mutable binding
        // kind for destructuring-param-introduced names.
        lower_pattern_bind(builder, ctx, pat, param_reg, false)?;
    }
    Ok(())
}

/// Materialises the rest parameter's array from
/// `activation.overflow_args` and binds it to a newly-allocated
/// local slot. Called at function entry after default
/// initializers.
///
/// `function f(a, b, ...rest)` — the runtime's `CallDirect` /
/// `CallProperty` copy only the non-rest args into parameter slots
/// (`param_count = 2` here); any additional arguments land in the
/// activation's `overflow_args`. `CreateRestParameters` drains
/// that into a fresh Array, which we then `Star` into `r_rest`.
///
/// The rest binding is a local (not a param slot) so it stays out
/// of the FrameLayout's `parameter_count` — that count matches the
/// runtime's arg-copy window.
fn emit_rest_parameter<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    layout: &ParamsLayout<'a>,
) -> Result<(), SourceLoweringError> {
    // Named rest — the simple `function f(...rest)` case.
    if let Some(rest_name) = layout.rest_name {
        // Allocate rest as a `const`-like local. ES spec treats
        // rest as a fresh binding (not a param alias).
        let slot = ctx.allocate_local(rest_name, true, Span::default())?;
        builder
            .emit(Opcode::CreateRestParameters, &[])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CreateRestParameters: {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode Star (rest): {err:?}")))?;
        ctx.mark_initialized(rest_name)?;
        return Ok(());
    }
    // Destructuring rest — `function f(...[a, b])` / `...{ a }`.
    // Build the rest array into an anonymous local, then let the
    // shared pattern-bind helper expand the pattern's leaves into
    // fresh user-visible locals.
    if let Some(pattern) = layout.rest_pattern {
        let slot = ctx.allocate_anonymous_local()?;
        builder
            .emit(Opcode::CreateRestParameters, &[])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode CreateRestParameters (destruct): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (destruct rest): {err:?}"))
            })?;
        lower_pattern_bind(builder, ctx, pattern, slot, true)?;
    }
    Ok(())
}

fn lower_function_body<'a>(
    body: &'a FunctionBody<'a>,
    params: &'a FormalParameters<'a>,
    layout: &ParamsLayout<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
) -> Result<FunctionBodyOutput, SourceLoweringError> {
    lower_function_body_with_parent(
        body,
        params,
        layout,
        function_names,
        module_functions,
        None,
        None,
        None,
    )
    .map(|(out, _captures)| out)
}

#[allow(clippy::too_many_arguments)]
fn lower_function_body_with_parent<'a>(
    body: &'a FunctionBody<'a>,
    _params: &'a FormalParameters<'a>,
    layout: &ParamsLayout<'a>,
    function_names: &'a [&'a str],
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    parent: Option<&'a LoweringContext<'a>>,
    class_super_binding: Option<ClassSuperBinding>,
    class_private_names: Option<std::rc::Rc<[String]>>,
) -> Result<(FunctionBodyOutput, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    // §14.1.1 Directive prologues — `"use strict"` is already the
    // default for ES modules, and classes / methods are strict
    // per spec regardless. Other string-literal directives are
    // silently ignored (the spec allows implementations to
    // reserve additional directive strings; nothing requires us
    // to honour them). Treat the whole prologue as metadata.
    let _ = &body.directives;

    let mut builder = BytecodeBuilder::new();
    let mut ctx = LoweringContext::with_parent(
        layout,
        function_names,
        module_functions,
        parent,
        class_super_binding,
        class_private_names,
    );

    // §14.1.21 FunctionDeclarationInstantiation — evaluate default
    // initializers for any param whose caller-supplied value is
    // `undefined`, then materialise the rest parameter's array
    // from `activation.overflow_args`. Both run before any body
    // statement so `Ldar r_param` later in the body sees a
    // definite value.
    emit_default_initializers(&mut builder, &mut ctx, layout)?;
    emit_param_destructuring(&mut builder, &mut ctx, layout)?;
    emit_rest_parameter(&mut builder, &mut ctx, layout)?;

    // Empty function body — synthesise `LdaUndefined; Return` so
    // the function exits per §15.2.1 FunctionBody evaluation
    // (falls through to `return undefined`). This lets
    // `function f() {}`, `() => {}`, and empty class-method
    // bodies all compile.
    let Some((last, rest)) = body.statements.split_last() else {
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined (empty body): {err:?}"))
        })?;
        builder.emit(Opcode::Return, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode Return (empty body): {err:?}"))
        })?;
        let exception_handlers = ctx.take_exception_handlers(&builder)?;
        let bytecode_len = builder.pc();
        let closure_table = ctx.take_closure_table(bytecode_len);
        let bytecode = builder
            .finish()
            .map_err(|err| SourceLoweringError::Internal(format!("finalise bytecode: {err:?}")))?;
        let captures = ctx.take_captures();
        return Ok((
            FunctionBodyOutput {
                bytecode,
                local_count: ctx.local_count(),
                temp_count: ctx.temp_count(),
                feedback_slot_count: ctx.feedback_slot_count(),
                feedback_slot_kinds: ctx.take_feedback_slot_kinds(),
                property_names: ctx.take_property_names(),
                float_constants: ctx.take_float_constants(),
                string_literals: ctx.take_string_literals(),
                bigint_constants: ctx.take_bigint_constants(),
                regexp_literals: ctx.take_regexp_literals(),
                exceptions: crate::exception::ExceptionTable::new(exception_handlers),
                closures: closure_table,
                source_map: ctx.take_source_map(),
            },
            captures,
        ));
    };

    // Two tail shapes are accepted:
    //   1. Explicit `return <expr>;` — lower the expression into
    //      acc, then `Return`. Matches the historical M6 contract.
    //   2. Any other statement — lower it as usual, then synthesize
    //      `LdaUndefined; Return` so the function exits with the
    //      undefined completion per §15.2.1 (FunctionBody evaluation
    //      falls through to `return undefined` when no explicit
    //      return is taken). This unlocks the natural
    //      `function main() { console.log("hi"); }` shape — prior
    //      to M19 the lowering required a spurious trailing
    //      `return` which is not how real JS is written.
    //
    // Bare `return;` with no argument is lowered by the second arm
    // because oxc represents it as a `ReturnStatement` with
    // `argument == None`, which `lower_nested_statement` handles as
    // `LdaUndefined; Return` directly.
    lower_function_top_statement_list(&mut builder, &mut ctx, rest)?;
    let needs_synthetic_return = match last {
        Statement::ReturnStatement(ret) if ret.argument.is_some() => {
            // D2: the trailing-return fast path bypasses
            // `lower_top_statement`, so record the source
            // location here to keep stack traces accurate for
            // the most common final statement.
            ctx.record_source_location(builder.pc(), last.span().start);
            let argument = ret.argument.as_ref().expect("checked Some above");
            lower_return_expression(&mut builder, &ctx, argument)?;
            builder
                .emit(Opcode::Return, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
            false
        }
        // Arrow concise body — oxc wraps `() => expr` as a
        // FunctionBody with a single `ExpressionStatement`
        // containing the expression. §15.3 specifies that this
        // form is semantically `() => { return expr; }`, so we
        // lower it as an implicit return. Detected by checking
        // that this is the ONLY statement in the body (no
        // preceding `rest`) and its expression can be any
        // acc-producing shape, not just call / assign / update.
        Statement::ExpressionStatement(expr_stmt)
            if rest.is_empty() && body.statements.len() == 1 =>
        {
            // Only take this path for expressions the top-statement
            // lowerer wouldn't already have accepted (call, assign,
            // update). For those we fall through to the default
            // catchall below, keeping the pre-existing semantics
            // (call expression statement leaves `undefined` as the
            // implicit return, matching regular function bodies).
            if matches!(
                expr_stmt.expression,
                Expression::CallExpression(_)
                    | Expression::AssignmentExpression(_)
                    | Expression::UpdateExpression(_)
            ) {
                lower_top_statement(&mut builder, &mut ctx, last)?;
                true
            } else {
                lower_return_expression(&mut builder, &ctx, &expr_stmt.expression)?;
                builder.emit(Opcode::Return, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Return (arrow concise body): {err:?}"
                    ))
                })?;
                false
            }
        }
        _ => {
            // Lower the statement (call-statement, assignment, if,
            // while, block, bare `return;`, …) — it must be a
            // shape `lower_top_statement` already accepts.
            lower_function_top_statement_list(&mut builder, &mut ctx, std::slice::from_ref(last))?;
            true
        }
    };
    if needs_synthetic_return {
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined (synth return): {err:?}"))
        })?;
        builder.emit(Opcode::Return, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode Return (synth): {err:?}"))
        })?;
    }

    // Resolve pending exception handlers to concrete PCs before
    // `finish()` drops the builder's label state.
    let exception_handlers = ctx.take_exception_handlers(&builder)?;
    let bytecode_len = builder.pc();
    let closure_table = ctx.take_closure_table(bytecode_len);

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finalise bytecode: {err:?}")))?;

    let captures = ctx.take_captures();
    Ok((
        FunctionBodyOutput {
            bytecode,
            local_count: ctx.local_count(),
            temp_count: ctx.temp_count(),
            feedback_slot_count: ctx.feedback_slot_count(),
            feedback_slot_kinds: ctx.take_feedback_slot_kinds(),
            property_names: ctx.take_property_names(),
            float_constants: ctx.take_float_constants(),
            string_literals: ctx.take_string_literals(),
            bigint_constants: ctx.take_bigint_constants(),
            regexp_literals: ctx.take_regexp_literals(),
            exceptions: crate::exception::ExceptionTable::new(exception_handlers),
            closures: closure_table,
            source_map: ctx.take_source_map(),
        },
        captures,
    ))
}

/// Lowers a single statement at function-body top level. Accepts the
/// full M6 statement surface, including `let`/`const` declarations
/// (which are not allowed inside nested blocks — those go through
/// [`lower_nested_statement`] instead).
pub(super) fn lower_top_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    match stmt {
        Statement::VariableDeclaration(decl) => {
            // D2: `let`/`const` bypass `lower_nested_statement` (the
            // central recording point), so record the starting PC
            // here to keep stack traces / debugger lookups aligned.
            ctx.record_source_location(builder.pc(), stmt.span().start);
            lower_let_const_declaration(builder, ctx, decl)
        }
        // `export const X = ...` at the top level — the compiler's
        // top-level classifier pushes the wrapping
        // `ExportNamedDeclaration` into the script body because
        // the inner `VariableDeclaration` can't be borrowed out of
        // the oxc arena separately. Unwrap it here so the `const`
        // initialiser runs and allocates a local; the synth
        // top-level then flushes the local onto the global object
        // before `capture_exports` harvests the namespace.
        Statement::ExportNamedDeclaration(decl) => {
            ctx.record_source_location(builder.pc(), stmt.span().start);
            match &decl.declaration {
                Some(Declaration::VariableDeclaration(inner)) => {
                    lower_let_const_declaration(builder, ctx, inner)
                }
                Some(Declaration::ClassDeclaration(cls)) => {
                    lower_nested_class_declaration(builder, ctx, cls)
                }
                // `export function` at the top level was already
                // recorded as a regular function declaration by
                // `lower_program` — the synth top-level doesn't
                // need to re-execute it here. Silent no-op.
                Some(Declaration::FunctionDeclaration(_)) | None => Ok(()),
                _ => Err(SourceLoweringError::unsupported(
                    "export_declaration_non_function",
                    stmt.span(),
                )),
            }
        }
        // §16.2.3 `export default …` — the outer wrapper is
        // pushed into `script_body` unchanged by `lower_program`
        // for every non-named-function shape. Dispatch by the
        // inner declaration kind:
        //
        // - Named class → same path as a top-level class decl;
        //   the class name is the export local.
        // - Named function → already registered as a regular
        //   top-level declaration; no-op at script time.
        // - Expression / anonymous → evaluate into acc and bind
        //   the result to `__otter_default` so the
        //   exported-const flush at the top-level tail installs
        //   it on the global object.
        Statement::ExportDefaultDeclaration(decl) => {
            ctx.record_source_location(builder.pc(), stmt.span().start);
            match &decl.declaration {
                ExportDefaultDeclarationKind::ClassDeclaration(cls) if cls.id.is_some() => {
                    lower_nested_class_declaration(builder, ctx, cls)
                }
                ExportDefaultDeclarationKind::FunctionDeclaration(func) if func.id.is_some() => {
                    let _ = func;
                    Ok(())
                }
                ExportDefaultDeclarationKind::ClassDeclaration(cls) => {
                    lower_class_expression(builder, ctx, cls)?;
                    lower_default_export_initializer(builder, ctx, stmt.span())
                }
                ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    lower_function_expression(builder, ctx, func)?;
                    lower_default_export_initializer(builder, ctx, stmt.span())
                }
                other => {
                    let expr = other.to_expression();
                    lower_return_expression(builder, ctx, expr)?;
                    lower_default_export_initializer(builder, ctx, stmt.span())
                }
            }
        }
        _ => lower_nested_statement(builder, ctx, stmt),
    }
}

/// Stores the current default-export value from acc into the
/// synthetic module-local binding used by anonymous default
/// declarations and default-export expressions.
///
/// Spec: https://tc39.es/ecma262/#sec-exports
fn lower_default_export_initializer<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    span: Span,
) -> Result<(), SourceLoweringError> {
    let slot = ctx.allocate_local(MODULE_DEFAULT_EXPORT_LOCAL, true, span)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (export default expr): {err:?}"))
        })?;
    ctx.mark_initialized(MODULE_DEFAULT_EXPORT_LOCAL)?;
    Ok(())
}

/// Lowers a single statement in a "nested" context (inside an `if`
/// branch, a `while` body, a `for` body, or a nested
/// `BlockStatement`). The accepted surface is a strict subset of
/// [`lower_top_statement`]: it does **not** allow `let`/`const`
/// declarations as a statement, since the compiler has no block
/// scoping and hoisting them to the surrounding function scope
/// would alter observable semantics. Inline `return` statements are
/// accepted (early-return pattern). `for (let …; …; …)` is special-
/// cased inside [`lower_for_statement`], which uses
/// [`LoweringContext::snapshot_scope`] / [`restore_scope`] to give
/// the for-init `let` a real lexical lifetime.
///
/// Takes `&mut ctx` so a `for` whose init is a `let` can call
/// `allocate_local` without an extra dispatch level.
pub(super) fn lower_nested_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    // D2: every statement starts at its AST span's byte offset.
    // Recording the PC about to be emitted (= current bytecode
    // length) → (line, column) gives the error reporter and
    // future debugger a precise anchor without touching any
    // expression-level helper. Finer granularity (per-opcode)
    // can layer on top of this later.
    ctx.record_source_location(builder.pc(), stmt.span().start);
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => {
            // Statement-position expressions: lower any value-
            // producing expression and discard the accumulator on
            // return. The common shapes (AssignmentExpression,
            // CallExpression, UpdateExpression) still take the
            // direct path to avoid the extra indirection, but
            // `delete obj.x;`, `obj.x;` (bare member read —
            // triggers a getter), `void expr;`, etc. also work.
            match &expr_stmt.expression {
                Expression::AssignmentExpression(assign) => {
                    lower_assignment_expression(builder, ctx, assign)
                }
                Expression::CallExpression(call) => lower_call_expression(builder, ctx, call),
                Expression::UpdateExpression(update) => {
                    lower_update_expression(builder, ctx, update)
                }
                _ => lower_return_expression(builder, ctx, &expr_stmt.expression),
            }
        }
        Statement::IfStatement(if_stmt) => lower_if_statement(builder, ctx, if_stmt),
        Statement::WhileStatement(while_stmt) => lower_while_statement(builder, ctx, while_stmt),
        Statement::DoWhileStatement(do_stmt) => lower_do_while_statement(builder, ctx, do_stmt),
        Statement::ForStatement(for_stmt) => lower_for_statement(builder, ctx, for_stmt),
        Statement::ForOfStatement(for_of) => lower_for_of_statement(builder, ctx, for_of),
        Statement::ForInStatement(for_in) => lower_for_in_statement(builder, ctx, for_in),
        Statement::SwitchStatement(sw) => lower_switch_statement(builder, ctx, sw),
        Statement::FunctionDeclaration(func) => {
            lower_nested_function_declaration(builder, ctx, func)
        }
        Statement::ClassDeclaration(class) => lower_nested_class_declaration(builder, ctx, class),
        Statement::ThrowStatement(throw) => lower_throw_statement(builder, ctx, throw),
        Statement::TryStatement(try_stmt) => lower_try_statement(builder, ctx, try_stmt),
        Statement::BreakStatement(break_stmt) => lower_break_statement(builder, ctx, break_stmt),
        Statement::ContinueStatement(cont_stmt) => {
            lower_continue_statement(builder, ctx, cont_stmt)
        }
        Statement::ReturnStatement(ret) => lower_return_statement(builder, ctx, ret),
        Statement::BlockStatement(block) => lower_block_statement(builder, ctx, block),
        Statement::LabeledStatement(labeled) => lower_labeled_statement(builder, ctx, labeled),
        Statement::VariableDeclaration(decl) => match decl.kind {
            // `var` is a valid statement-position body for `if` /
            // `while` / `do-while` / labelled statements. We already
            // lower `var` through the shared declaration path as a
            // declaration-site local, so reuse that here instead of
            // keeping the stale blanket rejection.
            VariableDeclarationKind::Var => lower_let_const_declaration(builder, ctx, decl),
            _ => Err(SourceLoweringError::unsupported(
                "parser_recovery_bare_nested_lexical_declaration",
                decl.span,
            )),
        },
        other => Err(SourceLoweringError::unsupported(
            statement_construct_tag(other),
            other.span(),
        )),
    }
}

/// Lowers a `BlockStatement` with its own lexical scope (M12).
///
/// A fresh scope snapshot brackets the block body so any `let` /
/// `const` declared inside the block pops off the locals stack on
/// exit. Slot reservations survive via
/// [`LoweringContext::peak_local_count`], matching the `for`-init
/// scoping model — bindings that came in between enter and exit
/// keep their frame slots allocated, so a later sibling block can't
/// reuse them (which would be visibly wrong if a closure snapshotted
/// the old slot).
///
/// Nested blocks compose naturally: each block pushes its own
/// snapshot, and the popped-but-reserved slots stack in LIFO order.
/// `let`/`const` in an `if` / `while` / `for` body is accepted only
/// through a `{ ... }` wrapper per the JS spec (lexical declarations
/// in a bare Statement position are a SyntaxError the parser
/// already rejects).
///
/// Non-declaration statements inside the block fall through to
/// [`lower_nested_statement`] so the full nested-statement surface —
/// `if` / `while` / `for` / `return` / `break` / `continue` / inner
/// blocks / expression statements — keeps working unchanged.
fn lower_block_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    block: &'a oxc_ast::ast::BlockStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let scope = ctx.snapshot_scope();
    let result = lower_nested_statement_list(builder, ctx, &block.body);
    ctx.restore_scope(scope);
    result
}

/// §14.13 `LabelName : Statement` — attaches a label to the
/// enclosed statement so `break labelName` / `continue labelName`
/// can target it.
///
/// - Iteration body (`for` / `while` / `do-while` / `for-of` /
///   `for-in`) or `switch`: the label is stashed on the context
///   via `set_pending_loop_label`; the nested lowerer consumes it
///   when it pushes its `LoopLabels` frame, so the stack stays a
///   single level deep.
/// - Anything else (a block, an expression statement, an `if`,
///   another labelled statement): a dedicated break-only frame
///   is pushed so `break labelName` from deep inside the body
///   jumps past the labelled statement. `continue labelName` in
///   that position is §14.11 invalid (no iteration target) and
///   reported as `undeclared_label`.
fn lower_labeled_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    labeled: &'a oxc_ast::ast::LabeledStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let name: std::rc::Rc<str> = std::rc::Rc::from(labeled.label.name.as_str());
    match &labeled.body {
        Statement::WhileStatement(_)
        | Statement::DoWhileStatement(_)
        | Statement::ForStatement(_)
        | Statement::ForOfStatement(_)
        | Statement::ForInStatement(_)
        | Statement::SwitchStatement(_) => {
            // Let the iteration / switch lowerer pick up the label.
            ctx.set_pending_loop_label(std::rc::Rc::clone(&name));
            lower_nested_statement(builder, ctx, &labeled.body)
        }
        _ => {
            // Break-only labelled statement — `break labelName`
            // jumps to the synthesized exit label, any other
            // control flow passes through.
            let break_label = builder.new_label();
            ctx.enter_loop(LoopLabels {
                break_label,
                continue_label: None,
                label: Some(std::rc::Rc::clone(&name)),
            });
            let result = lower_nested_statement(builder, ctx, &labeled.body);
            ctx.exit_loop();
            result?;
            builder.bind_label(break_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind labelled block exit: {err:?}"))
            })?;
            Ok(())
        }
    }
}

/// Lowers an `if (test) consequent` (with optional `else alternate`).
/// Bytecode shape:
///
/// ```text
/// without `else`:
///   <lower test>
///   JumpIfToBooleanFalse end_label
///   <lower consequent>
/// end_label:
///
/// with `else`:
///   <lower test>
///   JumpIfToBooleanFalse else_label
///   <lower consequent>
///   Jump end_label
/// else_label:
///   <lower alternate>
/// end_label:
/// ```
///
/// `JumpIfToBooleanFalse` performs JS truthy/falsy coercion so the
/// condition can be any value, not just a strict boolean — the
/// interpreter handles the `ToBoolean` step. Branches are lowered via
/// [`lower_nested_statement`] so they can themselves contain `if`s,
/// assignments, and inline `return`s.
fn lower_if_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    if_stmt: &'a oxc_ast::ast::IfStatement<'a>,
) -> Result<(), SourceLoweringError> {
    // Lower the condition into the accumulator. Reuses
    // `lower_return_expression` so any acc-producing expression
    // already supported (identifier, literal, binary, assignment,
    // parenthesised) works as a condition.
    lower_return_expression(builder, ctx, &if_stmt.test)?;

    let else_label = builder.new_label();
    builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, else_label)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse: {err:?}"))
        })?;

    lower_nested_statement(builder, ctx, &if_stmt.consequent)?;

    if let Some(alternate) = &if_stmt.alternate {
        let end_label = builder.new_label();
        builder
            .emit_jump_to(Opcode::Jump, end_label)
            .map_err(|err| SourceLoweringError::Internal(format!("encode Jump: {err:?}")))?;
        builder
            .bind_label(else_label)
            .map_err(|err| SourceLoweringError::Internal(format!("bind else label: {err:?}")))?;
        lower_nested_statement(builder, ctx, alternate)?;
        builder
            .bind_label(end_label)
            .map_err(|err| SourceLoweringError::Internal(format!("bind end label: {err:?}")))?;
    } else {
        builder
            .bind_label(else_label)
            .map_err(|err| SourceLoweringError::Internal(format!("bind else label: {err:?}")))?;
    }

    Ok(())
}

/// Lowers a `while (test) body` statement. Bytecode shape:
///
/// ```text
/// loop_header:
///   <lower test>
///   JumpIfToBooleanFalse loop_exit
///   <lower body>
///   Jump loop_header
/// loop_exit:
/// ```
///
/// The `Jump loop_header` at the bottom is a backward branch — the
/// dispatcher's tier-up budget decrements on every backward jump, so
/// the loop body accrues hotness exactly the way the JIT expects.
/// `break` and `continue` (unlabelled) are supported via the
/// `LoopLabels` stack: `break` forward-jumps to `loop_exit`, and
/// `continue` backward-jumps to `loop_header`. Labelled jumps are
/// rejected. The body is lowered via [`lower_nested_statement`] so
/// it can contain assignments, nested `if`/`while`, blocks, and
/// inline `return`s — but no `let`/`const` (block scoping lands
/// later).
fn lower_while_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    while_stmt: &'a oxc_ast::ast::WhileStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let loop_header = builder.new_label();
    let loop_exit = builder.new_label();

    builder
        .bind_label(loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("bind loop header: {err:?}")))?;

    lower_return_expression(builder, ctx, &while_stmt.test)?;
    builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, loop_exit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse: {err:?}"))
        })?;

    // Register this loop's jump targets so any nested `break` /
    // `continue` can find them. `while` uses the loop header as the
    // continue target — re-running the test is the spec-correct
    // semantics.
    ctx.enter_loop(LoopLabels {
        break_label: loop_exit,
        continue_label: Some(loop_header),
        label: ctx.take_pending_loop_label(),
    });
    let body_result = lower_nested_statement(builder, ctx, &while_stmt.body);
    ctx.exit_loop();
    body_result?;

    builder
        .emit_jump_to(Opcode::Jump, loop_header)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Jump (loop back): {err:?}"))
        })?;
    builder
        .bind_label(loop_exit)
        .map_err(|err| SourceLoweringError::Internal(format!("bind loop exit: {err:?}")))?;

    Ok(())
}

/// §14.7.2 `do { body } while (test)` — test runs *after* the body,
/// so the body always executes at least once. Bytecode shape:
///
/// ```text
/// loop_header:
///   <lower body>
/// continue_target:
///   <lower test>
///   JumpIfToBooleanTrue loop_header
/// loop_exit:
/// ```
///
/// `continue` jumps past the body to re-run the test (per spec),
/// `break` exits the loop entirely.
fn lower_do_while_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    do_stmt: &'a oxc_ast::ast::DoWhileStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let loop_header = builder.new_label();
    let continue_target = builder.new_label();
    let loop_exit = builder.new_label();

    builder
        .bind_label(loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("bind do-while header: {err:?}")))?;

    ctx.enter_loop(LoopLabels {
        break_label: loop_exit,
        continue_label: Some(continue_target),
        label: ctx.take_pending_loop_label(),
    });
    let body_result = lower_nested_statement(builder, ctx, &do_stmt.body);
    ctx.exit_loop();
    body_result?;

    builder
        .bind_label(continue_target)
        .map_err(|err| SourceLoweringError::Internal(format!("bind do-while continue: {err:?}")))?;
    lower_return_expression(builder, ctx, &do_stmt.test)?;
    builder
        .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_header)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanTrue (do-while): {err:?}"))
        })?;
    builder
        .bind_label(loop_exit)
        .map_err(|err| SourceLoweringError::Internal(format!("bind do-while exit: {err:?}")))?;

    Ok(())
}

/// Lowers a `for (init; test; update) body` statement. Bytecode shape:
///
/// ```text
///   <lower init>           ; let / const / assignment / nothing
/// loop_header:
///   <lower test>           ; or LdaTrue when omitted
///   JumpIfToBooleanFalse loop_exit
///   <lower body>
///   <lower update>         ; or no-op when omitted
///   Jump loop_header
/// loop_exit:
/// ```
///
/// Equivalent to the standard `for → while` desugaring:
///
/// ```text
///   { <init>; while (<test>) { <body>; <update>; } }
/// ```
///
/// `for (let i = …; …; …)` scopes the init binding to the loop —
/// uses [`LoweringContext::snapshot_scope`] / [`restore_scope`] to
/// pop the binding on loop exit while keeping the FrameLayout's
/// reservation in place. `for (;;)` is accepted; the body must
/// contain a `return` to terminate (no `break` yet). `for (… in …)`
/// and `for (… of …)` are separate AST node types and rejected with
/// their own tags.
fn lower_for_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_stmt: &'a oxc_ast::ast::ForStatement<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ForStatementInit;

    if let Some(ForStatementInit::VariableDeclaration(decl)) = &for_stmt.init
        && matches!(
            decl.kind,
            VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
        )
    {
        return lower_classic_for_using_statement(builder, ctx, for_stmt, decl);
    }

    // Snapshot scope so any `let` introduced by the init pops on exit.
    let scope = ctx.snapshot_scope();

    // 1) Init.
    if let Some(init) = &for_stmt.init {
        match init {
            ForStatementInit::VariableDeclaration(decl) => {
                lower_let_const_declaration(builder, ctx, decl)?;
            }
            // `for (i = 0; …)` — init inherits the `Expression`
            // variants. Only an assignment expression makes sense at
            // statement-equivalent position; anything else (bare
            // read, call, comma) is rejected with a stable tag.
            ForStatementInit::AssignmentExpression(assign) => {
                lower_assignment_expression(builder, ctx, assign)?;
            }
            // Any other expression-shaped init (call, update,
            // sequence, etc.) — lower for side effects, discard
            // the accumulator. `ForStatementInit` inherits every
            // `Expression` variant via oxc's `inherit_variants!`
            // macro, so `to_expression()` gives us the borrowed
            // Expression to run through the regular lowerer.
            other => {
                lower_return_expression(builder, ctx, other.to_expression())?;
            }
        }
    }

    let loop_header = builder.new_label();
    let loop_exit = builder.new_label();
    // `continue` in a `for` jumps to the update clause (or the
    // loop header when there's no update). Using a dedicated
    // `loop_continue` label lets both paths share the bind sequence
    // below without leaking the difference to callers.
    let loop_continue = builder.new_label();

    builder
        .bind_label(loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for header: {err:?}")))?;

    // 2) Test. Omitted test ⇒ unconditional loop, lowered as
    //    `LdaTrue` so the `JumpIfToBooleanFalse` path stays uniform
    //    with `while`. The interpreter / JIT can fold the constant-
    //    true branch later; emitting it now keeps the bytecode
    //    shape predictable for the v2 dispatcher.
    if let Some(test) = &for_stmt.test {
        lower_return_expression(builder, ctx, test)?;
    } else {
        builder
            .emit(Opcode::LdaTrue, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode LdaTrue: {err:?}")))?;
    }
    builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, loop_exit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse: {err:?}"))
        })?;

    // 3) Body. Register the loop frame first so nested
    //    `break` / `continue` pick up our labels; pop after the
    //    body lowering completes.
    ctx.enter_loop(LoopLabels {
        break_label: loop_exit,
        continue_label: Some(loop_continue),
        label: ctx.take_pending_loop_label(),
    });
    let body_result = lower_nested_statement(builder, ctx, &for_stmt.body);
    ctx.exit_loop();
    body_result?;

    // 4) Continue target — runs the update clause (if any) and then
    //    falls through to the back-jump. `continue` from the body
    //    lands here, so the update still executes per spec.
    builder
        .bind_label(loop_continue)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for continue: {err:?}")))?;

    // 5) Update — runs after every iteration, before the back-jump.
    //    M10 also accepts `UpdateExpression` (`i++` / `++i`),
    //    matching the canonical `for (let i = 0; i < n; i++)` idiom.
    //    The UpdateExpression's accumulator result is discarded.
    if let Some(update) = &for_stmt.update {
        match update {
            Expression::AssignmentExpression(assign) => {
                lower_assignment_expression(builder, ctx, assign)?;
            }
            Expression::UpdateExpression(update_expr) => {
                lower_update_expression(builder, ctx, update_expr)?;
            }
            Expression::CallExpression(call) => lower_call_expression(builder, ctx, call)?,
            // Any other expression in the update slot — lower and
            // discard. `for (let i = 0; i < n; log(i), i++)` uses
            // a SequenceExpression; `for (…; …; obj.method())` is
            // the CallExpression case already above.
            other => {
                lower_return_expression(builder, ctx, other)?;
            }
        }
    }

    builder
        .emit_jump_to(Opcode::Jump, loop_header)
        .map_err(|err| SourceLoweringError::Internal(format!("encode Jump (for back): {err:?}")))?;
    builder
        .bind_label(loop_exit)
        .map_err(|err| SourceLoweringError::Internal(format!("bind for exit: {err:?}")))?;

    ctx.restore_scope(scope);
    Ok(())
}

/// M30: lowers `for (<left> of <iterable>) <body>`.
///
/// Bytecode shape:
///
/// ```text
///   <lower iterable> → acc
///   Star r_src
///   GetIterator r_src → acc = iterator
///   Star r_iter
/// loop_top:                    ; also `continue` target
///   IteratorStep r_binding r_iter
///     ; writes done → acc, value → r_binding when not done
///   JumpIfToBooleanTrue loop_exit
///   <lower body>
///   Jump loop_top
/// loop_exit:
/// ```
///
/// Left-hand side forms supported in M30:
/// - `let x` / `const x` — fresh binding scoped to the loop body
///   (note: the M30 lowering reuses one slot per iteration;
///   spec-accurate CreatePerIterationEnvironment is a follow-up,
///   relevant only for body closures that capture the binding).
/// - plain `Identifier` target — assigns to an existing binding,
///   including a captured outer binding.
///
/// Deferred to later milestones: `for await`, destructuring
/// patterns in `left`, iterator-close on abrupt completion
/// (`break` / `return` through a custom iterator), async
/// iterators.
fn lower_for_of_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_of: &'a oxc_ast::ast::ForOfStatement<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ForStatementLeft;
    use oxc_ast::ast::VariableDeclarationKind;
    if for_of.r#await {
        return Err(SourceLoweringError::unsupported(
            "for_await_of_statement",
            for_of.span,
        ));
    }

    // Snapshot scope so any `let` bindings introduced by `left`
    // pop on loop exit — mirrors how `for` init bindings work.
    let scope = ctx.snapshot_scope();

    // 1) Reserve iterator bookkeeping slots as hidden locals.
    //    Nested `for…of` loops shift `peak_local_count` upward as
    //    inner body bindings are allocated, so the iterator + src
    //    registers must live in the locals region rather than the
    //    temp region. Using `allocate_anonymous_local` keeps them
    //    safe from later `let`/`const` allocations inside the body.
    let src_local = ctx.allocate_anonymous_local()?;
    let iter_local = ctx.allocate_anonymous_local()?;
    let src_temp = src_local;
    let iter_temp = iter_local;

    let result = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &for_of.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-of iterable): {err:?}"))
            })?;
        builder
            .emit(Opcode::GetIterator, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode GetIterator: {err:?}")))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(iter_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-of iterator): {err:?}"))
            })?;

        // 2) Resolve the binding register. Three shapes:
        //    - `let x` / `const x`: allocate a fresh local.
        //    - `let [a, b]` / `let { x }`: allocate an anonymous
        //      local to hold each iteration's value; a
        //      destructuring pattern-bind runs before the body.
        //    - `x` (identifier assignment): reuse the existing
        //      binding's register, or spill through a hidden
        //      local before storing into an upvalue.
        let mut destructuring_pattern: Option<(&BindingPattern<'a>, bool)> = None;
        let mut assignment_target: Option<ForInOfAssignmentTarget<'a>> = None;
        let mut upvalue_target: Option<(u16, u16)> = None;
        let mut loop_using_await_dispose: Option<bool> = None;
        let (binding_reg, is_let_like) = match &for_of.left {
            ForStatementLeft::VariableDeclaration(decl) => {
                // `var`, `let`, and `const` all flow through the
                // same allocate-local + per-iteration store path
                // for the for-of target. `var` stays
                // block-scoped-like here until full function
                // hoisting lands — same compromise as plain
                // `var` declarations elsewhere.
                if decl.declarations.len() != 1 {
                    return Err(SourceLoweringError::unsupported(
                        "for_of_multiple_bindings",
                        decl.span,
                    ));
                }
                let declarator = &decl.declarations[0];
                if declarator.init.is_some() {
                    return Err(SourceLoweringError::unsupported(
                        "for_of_binding_initializer",
                        declarator.span,
                    ));
                }
                let is_using = matches!(
                    decl.kind,
                    VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing
                );
                let is_const = decl.kind == VariableDeclarationKind::Const || is_using;
                if is_using {
                    loop_using_await_dispose =
                        Some(decl.kind == VariableDeclarationKind::AwaitUsing);
                }
                match &declarator.id {
                    oxc_ast::ast::BindingPattern::BindingIdentifier(ident) => {
                        let name = ident.name.as_str();
                        let slot = ctx.allocate_local(name, is_const, declarator.span)?;
                        ctx.mark_initialized(name)?;
                        (slot, true)
                    }
                    // Destructuring for-of target: allocate an
                    // anonymous hidden local to hold the per-
                    // iteration value, then run the pattern bind
                    // against it once we enter the body.
                    oxc_ast::ast::BindingPattern::ArrayPattern(_)
                    | oxc_ast::ast::BindingPattern::ObjectPattern(_)
                        if !is_using =>
                    {
                        let iter_val_slot = ctx.allocate_anonymous_local()?;
                        destructuring_pattern = Some((&declarator.id, is_const));
                        (iter_val_slot, true)
                    }
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            if is_using {
                                "parser_recovery_for_of_using_pattern"
                            } else {
                                "for_of_destructuring_binding"
                            },
                            other.span(),
                        ));
                    }
                }
            }
            _ => match classify_for_in_of_left(&for_of.left, "parser_recovery_for_of_lhs")? {
                ForInOfLeft::Identifier(ident) => {
                    let name = ident.name.as_str();
                    let binding = ctx.resolve_identifier(name).ok_or_else(|| {
                        SourceLoweringError::unsupported("unbound_identifier", ident.span)
                    })?;
                    match binding {
                        BindingRef::Local {
                            reg,
                            initialized: true,
                            is_const: false,
                            ..
                        } => (reg, false),
                        BindingRef::Param { reg } => (reg, false),
                        BindingRef::Local { is_const: true, .. } => {
                            return Err(SourceLoweringError::unsupported(
                                "const_assignment",
                                ident.span,
                            ));
                        }
                        BindingRef::Local {
                            initialized: false, ..
                        } => {
                            return Err(SourceLoweringError::unsupported(
                                "tdz_self_reference",
                                ident.span,
                            ));
                        }
                        BindingRef::Upvalue { idx } => {
                            let iter_val_slot = ctx.allocate_anonymous_local()?;
                            upvalue_target = Some((iter_val_slot, idx));
                            (iter_val_slot, false)
                        }
                    }
                }
                ForInOfLeft::AssignmentTarget(target) => {
                    let iter_val_slot = ctx.allocate_anonymous_local()?;
                    assignment_target = Some(target);
                    (iter_val_slot, false)
                }
            },
        };
        let _ = is_let_like;

        // 3) Loop skeleton.
        let loop_top = builder.new_label();
        let loop_exit = builder.new_label();
        builder
            .bind_label(loop_top)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-of top: {err:?}")))?;
        builder
            .emit(
                Opcode::IteratorStep,
                &[
                    Operand::Reg(u32::from(binding_reg)),
                    Operand::Reg(u32::from(iter_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode IteratorStep: {err:?}"))
            })?;
        builder
            .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_exit)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfToBooleanTrue (for-of done): {err:?}"
                ))
            })?;
        if let Some((iter_val_reg, upvalue_idx)) = upvalue_target {
            lower_for_in_of_upvalue_assignment(builder, iter_val_reg, upvalue_idx)?;
        }
        if let Some(target) = assignment_target {
            lower_for_in_of_assignment_target(builder, ctx, target, binding_reg)?;
        }

        // 4) Body. Register loop labels so nested
        //    `break` / `continue` target our skeleton — `continue`
        //    resumes at the iterator-step, `break` jumps past
        //    the loop.
        ctx.enter_loop(LoopLabels {
            break_label: loop_exit,
            continue_label: Some(loop_top),
            label: ctx.take_pending_loop_label(),
        });
        let body_result = if let Some(await_dispose) = loop_using_await_dispose {
            lower_loop_using_iteration(builder, ctx, binding_reg, await_dispose, |builder, ctx| {
                lower_nested_statement(builder, ctx, &for_of.body)
            })
        } else {
            (|| -> Result<(), SourceLoweringError> {
                // Destructuring for-of: expand the pattern against
                // the iterator value now in `binding_reg` so every
                // leaf becomes a fresh per-iteration local.
                if let Some((pattern, is_const)) = destructuring_pattern {
                    lower_pattern_bind(builder, ctx, pattern, binding_reg, is_const)?;
                }
                lower_nested_statement(builder, ctx, &for_of.body)
            })()
        };
        ctx.exit_loop();
        body_result?;

        builder
            .emit_jump_to(Opcode::Jump, loop_top)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (for-of back): {err:?}"))
            })?;
        builder
            .bind_label(loop_exit)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-of exit: {err:?}")))?;
        Ok(())
    })();

    ctx.restore_scope(scope);
    result
}

/// Performs ForIn/OfBodyEvaluation's assignment step for
/// `for (x of iterable)` / `for (x in object)` when `x` resolves
/// to an upvalue.
///
/// Spec: https://tc39.es/ecma262/#sec-runtime-semantics-forin-div-ofbodyevaluation-lhs-stmt-iterator-lhskind-labelset
fn lower_for_in_of_upvalue_assignment(
    builder: &mut BytecodeBuilder,
    iter_value_reg: u16,
    upvalue_idx: u16,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(iter_value_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (for-of upvalue target): {err:?}"))
        })?;
    builder
        .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(upvalue_idx))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode StaUpvalue (for-of upvalue target): {err:?}"
            ))
        })?;
    Ok(())
}

/// M31: lowers `for (<left> in <source>) <body>` — §14.7.5.11
/// ForInOfStatement, `in` variant. Walks the source's own +
/// inherited enumerable string-keyed property names via the
/// runtime's property iterator (allocated by `ForInEnumerate`,
/// stepped by `ForInNext`).
///
/// Bytecode shape:
///
/// ```text
///   <lower source> → acc
///   Star r_src
///   ForInEnumerate r_src → acc = property_iterator
///   Star r_iter
/// loop_top:
///   ForInNext r_binding r_iter
///     ; writes done → acc, key → r_binding when not done
///   JumpIfToBooleanTrue loop_exit
///   <lower body>
///   Jump loop_top
/// loop_exit:
/// ```
///
/// `null` / `undefined` sources don't throw — `ForInEnumerate`
/// allocates an empty iterator per §14.7.5.6 step 6, so the
/// body never runs.
///
/// Supported LHS forms mirror `for…of`: `let x` / `const x`
/// (fresh per-loop binding) and plain identifier targets,
/// including captured outer bindings. Same deferrals apply
/// (destructuring assignment targets, `var` hoisting details).
fn lower_for_in_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    for_in: &'a oxc_ast::ast::ForInStatement<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ForStatementLeft;
    use oxc_ast::ast::VariableDeclarationKind;

    // Snapshot scope so any `let` bindings introduced by `left`
    // pop on loop exit.
    let scope = ctx.snapshot_scope();

    // Reserve iterator bookkeeping slots as hidden locals (same
    // reasoning as `for…of` — nested loops shift the temp base
    // and would clobber temp-region temps).
    let src_local = ctx.allocate_anonymous_local()?;
    let iter_local = ctx.allocate_anonymous_local()?;

    let result = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &for_in.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_local))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-in source): {err:?}"))
            })?;
        builder
            .emit(
                Opcode::ForInEnumerate,
                &[Operand::Reg(u32::from(src_local))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode ForInEnumerate: {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(iter_local))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (for-in iterator): {err:?}"))
            })?;

        let mut for_in_destructuring_pattern: Option<(&BindingPattern<'a>, bool)> = None;
        let mut assignment_target: Option<ForInOfAssignmentTarget<'a>> = None;
        let mut upvalue_target: Option<(u16, u16)> = None;
        let binding_reg = match &for_in.left {
            ForStatementLeft::VariableDeclaration(decl) => {
                // `var`, `let`, `const` all allocate the same
                // per-loop local; function-scope hoisting for the
                // `var` flavour is still tracked as a follow-up.
                if decl.declarations.len() != 1 {
                    return Err(SourceLoweringError::unsupported(
                        "for_in_multiple_bindings",
                        decl.span,
                    ));
                }
                let declarator = &decl.declarations[0];
                if declarator.init.is_some() {
                    return Err(SourceLoweringError::unsupported(
                        "for_in_binding_initializer",
                        declarator.span,
                    ));
                }
                let is_const = decl.kind == VariableDeclarationKind::Const;
                match &declarator.id {
                    oxc_ast::ast::BindingPattern::BindingIdentifier(ident) => {
                        let name = ident.name.as_str();
                        let slot = ctx.allocate_local(name, is_const, declarator.span)?;
                        ctx.mark_initialized(name)?;
                        slot
                    }
                    oxc_ast::ast::BindingPattern::ArrayPattern(_)
                    | oxc_ast::ast::BindingPattern::ObjectPattern(_) => {
                        // `for (const { k } in obj)` — stash the
                        // per-iteration KEY in an anon local, run
                        // the destructure against it at the top of
                        // the body. For-in keys are strings, so
                        // destructuring is unusual but still valid.
                        let iter_val_slot = ctx.allocate_anonymous_local()?;
                        for_in_destructuring_pattern = Some((&declarator.id, is_const));
                        iter_val_slot
                    }
                    _ => {
                        return Err(SourceLoweringError::unsupported(
                            "for_in_destructuring_binding",
                            declarator.span,
                        ));
                    }
                }
            }
            _ => match classify_for_in_of_left(&for_in.left, "parser_recovery_for_in_lhs")? {
                ForInOfLeft::Identifier(ident) => {
                    let name = ident.name.as_str();
                    let binding = ctx.resolve_identifier(name).ok_or_else(|| {
                        SourceLoweringError::unsupported("unbound_identifier", ident.span)
                    })?;
                    match binding {
                        BindingRef::Local {
                            reg,
                            initialized: true,
                            is_const: false,
                            ..
                        } => reg,
                        BindingRef::Param { reg } => reg,
                        BindingRef::Local { is_const: true, .. } => {
                            return Err(SourceLoweringError::unsupported(
                                "const_assignment",
                                ident.span,
                            ));
                        }
                        BindingRef::Local {
                            initialized: false, ..
                        } => {
                            return Err(SourceLoweringError::unsupported(
                                "tdz_self_reference",
                                ident.span,
                            ));
                        }
                        BindingRef::Upvalue { idx } => {
                            let iter_val_slot = ctx.allocate_anonymous_local()?;
                            upvalue_target = Some((iter_val_slot, idx));
                            iter_val_slot
                        }
                    }
                }
                ForInOfLeft::AssignmentTarget(target) => {
                    let iter_val_slot = ctx.allocate_anonymous_local()?;
                    assignment_target = Some(target);
                    iter_val_slot
                }
            },
        };

        let loop_top = builder.new_label();
        let loop_exit = builder.new_label();
        builder
            .bind_label(loop_top)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-in top: {err:?}")))?;
        builder
            .emit(
                Opcode::ForInNext,
                &[
                    Operand::Reg(u32::from(binding_reg)),
                    Operand::Reg(u32::from(iter_local)),
                ],
            )
            .map_err(|err| SourceLoweringError::Internal(format!("encode ForInNext: {err:?}")))?;
        builder
            .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_exit)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfToBooleanTrue (for-in done): {err:?}"
                ))
            })?;
        if let Some((iter_val_reg, upvalue_idx)) = upvalue_target {
            lower_for_in_of_upvalue_assignment(builder, iter_val_reg, upvalue_idx)?;
        }
        if let Some(target) = assignment_target {
            lower_for_in_of_assignment_target(builder, ctx, target, binding_reg)?;
        }

        ctx.enter_loop(LoopLabels {
            break_label: loop_exit,
            continue_label: Some(loop_top),
            label: ctx.take_pending_loop_label(),
        });
        let body_result = (|| -> Result<(), SourceLoweringError> {
            if let Some((pattern, is_const)) = for_in_destructuring_pattern {
                lower_pattern_bind(builder, ctx, pattern, binding_reg, is_const)?;
            }
            lower_nested_statement(builder, ctx, &for_in.body)
        })();
        ctx.exit_loop();
        body_result?;

        builder
            .emit_jump_to(Opcode::Jump, loop_top)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (for-in back): {err:?}"))
            })?;
        builder
            .bind_label(loop_exit)
            .map_err(|err| SourceLoweringError::Internal(format!("bind for-in exit: {err:?}")))?;
        Ok(())
    })();

    ctx.restore_scope(scope);
    result
}

/// Lowers `switch (e) { case v: …; default: …; }`. Bytecode shape:
///
/// ```text
///   <lower discriminant into acc>
///   Star r_disc                        ; r_disc = discriminant
///   ; Compare phase — one dispatch per case, in source order.
///   Ldar r_disc                        ; acc = discriminant
///   TestEqualStrict r_v0               ; acc = (discriminant === v0)
///   JumpIfToBooleanTrue case_0
///   Ldar r_disc
///   TestEqualStrict r_v1
///   JumpIfToBooleanTrue case_1
///   …
///   Jump default_label                 ; or `switch_exit` if no default
///   ; Body phase — labels sit above each case's statements, in source
///   ; order, so fall-through between cases works naturally. `break`
///   ; inside a case targets `switch_exit`.
/// case_0:
///   <lower case 0 consequent>
/// case_1:
///   <lower case 1 consequent>
///   …
/// default_label:
///   <lower default consequent>
/// switch_exit:
/// ```
///
/// Each case-value expression is lowered into acc and spilled into
/// its own temp before the compare phase — this keeps the
/// discriminant fresh in `r_disc` across comparisons and lets the
/// `TestEqualStrict` opcode read `acc = discriminant` and
/// `r_value` directly without extra reloads.
///
/// §14.11 SwitchStatement — `break` exits the switch; `continue`
/// walks past the switch to the enclosing loop.
///
/// Intentionally simple: no jump-table optimisation for dense
/// int32 cases, no deduplication of duplicate case values. Those
/// are JIT-level tricks that land when the bytecode surface
/// stabilises.
fn lower_switch_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    sw: &'a oxc_ast::ast::SwitchStatement<'a>,
) -> Result<(), SourceLoweringError> {
    let switch_scope = enter_switch_lexical_scope(builder, ctx, sw)?;
    // 1) Evaluate discriminant into a temp. The compare phase
    //    reloads it before each `TestEqualStrict` so the acc is
    //    predictable when entering the comparison opcode.
    let disc_temp = ctx.acquire_temps(1)?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &sw.discriminant)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(disc_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (switch discriminant): {err:?}"))
            })?;

        // 2) Lower each `case <v>:` value into its own temp. We
        //    do this eagerly so the comparisons below can just
        //    `TestEqualStrict r_vN` without any re-evaluation.
        //    `default:` (test == None) doesn't consume a temp.
        let case_count = sw.cases.len();
        // Per-case labels — bound later above each case's body.
        let case_labels: Vec<Label> = (0..case_count).map(|_| builder.new_label()).collect();
        let switch_exit = builder.new_label();

        // Compute how many non-default cases we have so we can
        // acquire exactly that many value-temps.
        let value_case_count: u16 = sw
            .cases
            .iter()
            .filter(|c| c.test.is_some())
            .count()
            .try_into()
            .map_err(|_| SourceLoweringError::Internal("switch case count exceeds u16".into()))?;
        let value_base = if value_case_count == 0 {
            0
        } else {
            ctx.acquire_temps(value_case_count)?
        };

        let body_result = (|| -> Result<(), SourceLoweringError> {
            // Lower case values into consecutive temps. Index into
            // `value_base` advances only for non-default cases.
            let mut value_slot: u16 = 0;
            for case in sw.cases.iter() {
                let Some(test) = case.test.as_ref() else {
                    continue; // default — no value to evaluate.
                };
                lower_return_expression(builder, ctx, test)?;
                let slot = value_base.checked_add(value_slot).ok_or_else(|| {
                    SourceLoweringError::Internal("switch case value slot overflow".into())
                })?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (switch case value): {err:?}"
                        ))
                    })?;
                value_slot = value_slot
                    .checked_add(1)
                    .ok_or_else(|| SourceLoweringError::Internal("value_slot overflow".into()))?;
            }

            // 3) Compare phase. For each case with a test, emit
            //    `Ldar r_disc; TestEqualStrict r_vN;
            //    JumpIfToBooleanTrue case_label`. Default cases
            //    are skipped here and covered by the "no-match"
            //    fallback jump below.
            let mut value_slot: u16 = 0;
            let mut default_index: Option<usize> = None;
            for (case_idx, case) in sw.cases.iter().enumerate() {
                let Some(_test) = case.test.as_ref() else {
                    default_index = Some(case_idx);
                    continue;
                };
                builder
                    .emit(Opcode::Ldar, &[Operand::Reg(u32::from(disc_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Ldar (switch disc reload): {err:?}"
                        ))
                    })?;
                let value_reg = value_base.checked_add(value_slot).ok_or_else(|| {
                    SourceLoweringError::Internal("switch value reg overflow".into())
                })?;
                builder
                    .emit(
                        Opcode::TestEqualStrict,
                        &[Operand::Reg(u32::from(value_reg))],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode TestEqualStrict (switch): {err:?}"
                        ))
                    })?;
                builder
                    .emit_jump_to(Opcode::JumpIfToBooleanTrue, case_labels[case_idx])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode JumpIfToBooleanTrue (switch): {err:?}"
                        ))
                    })?;
                value_slot = value_slot
                    .checked_add(1)
                    .ok_or_else(|| SourceLoweringError::Internal("value_slot overflow".into()))?;
            }

            // 4) No case matched — jump to `default` if present,
            //    otherwise skip the entire body to `switch_exit`.
            let fallback = match default_index {
                Some(idx) => case_labels[idx],
                None => switch_exit,
            };
            builder
                .emit_jump_to(Opcode::Jump, fallback)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Jump (switch fallback): {err:?}"))
                })?;

            // 5) Body phase. `enter_loop` pushes the break-only
            //    frame so any nested `break` in a case jumps to
            //    `switch_exit`; `continue` walks past this frame
            //    because `continue_label` is `None`.
            ctx.enter_loop(LoopLabels {
                break_label: switch_exit,
                continue_label: None,
                label: ctx.take_pending_loop_label(),
            });

            let lower_cases = (|| -> Result<(), SourceLoweringError> {
                for (case_idx, case) in sw.cases.iter().enumerate() {
                    builder.bind_label(case_labels[case_idx]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "bind switch case {case_idx}: {err:?}"
                        ))
                    })?;
                    for stmt in case.consequent.iter() {
                        lower_switch_case_statement(builder, ctx, stmt)?;
                    }
                }
                Ok(())
            })();
            ctx.exit_loop();
            lower_cases?;

            // 6) Exit label — bound after all case bodies so fall
            //    through to the bottom is a natural next instruction.
            builder.bind_label(switch_exit).map_err(|err| {
                SourceLoweringError::Internal(format!("bind switch exit: {err:?}"))
            })?;
            Ok(())
        })();
        if value_case_count > 0 {
            ctx.release_temps(value_case_count);
        }
        body_result
    })();
    ctx.release_temps(1); // disc_temp
    ctx.restore_scope(switch_scope);
    lower
}

/// Lowers `throw <expr>;`. Evaluates the argument into acc, emits
/// `Opcode::Throw`, and lets the interpreter's throw-transfer path
/// find the nearest enclosing handler in the function's
/// `ExceptionTable`.
///
/// §14.14 ThrowStatement.
fn lower_throw_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    throw: &'a oxc_ast::ast::ThrowStatement<'a>,
) -> Result<(), SourceLoweringError> {
    lower_return_expression(builder, ctx, &throw.argument)?;
    builder
        .emit(Opcode::Throw, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Throw: {err:?}")))?;
    Ok(())
}

/// Resolved binding for a JS identifier reference. Mirrors the
/// `[hidden | params | locals]` frame layout: `Param.reg` is the
/// user-visible register index of the parameter (0 for the sole M5
/// parameter), `Local.reg` is the user-visible index of the
/// `let`/`const` slot. `initialized: false` flags a binding whose
/// own initializer is currently being lowered — reading it would be
/// a TDZ self-reference and is rejected at compile time. `is_const`
/// distinguishes `const` from `let`; M5's assignment lowering refuses
/// writes to const bindings.
#[derive(Debug, Clone, Copy)]
enum BindingRef {
    Param {
        reg: u16,
    },
    Local {
        reg: u16,
        initialized: bool,
        is_const: bool,
        runtime_tdz: bool,
    },
    /// M25: binding resolved in an enclosing scope — accessed
    /// through the inner closure's upvalue list. `idx` is the
    /// `LdaUpvalue`/`StaUpvalue` operand (0-based in capture
    /// order).
    Upvalue {
        idx: u16,
    },
}

/// In-scope `let`/`const` binding. The slot is assigned at allocation
/// time and stays stable for the binding's whole lifetime (M5 has no
/// shadowing or block scopes — those land with `IfStatement` /
/// `WhileStatement` in M6 / M7). `initialized` flips to `true` after
/// `Star r_slot` runs the post-init assignment; `is_const` is set
/// from the declaration kind and is used by `lower_assignment_expression`
/// to reject const writes.
#[derive(Debug)]
struct LocalBinding<'a> {
    name: &'a str,
    slot: u16,
    initialized: bool,
    is_const: bool,
    runtime_tdz: bool,
}

/// Per-function lowering context: tracks parameters (0..N regular
/// plus an optional rest param that lives as a local), every
/// `let`/`const` declared so far (with their assigned register
/// slots and TDZ state), the call-arg temp pool, and the shared
/// module-level function name table for resolving `CallExpression`
/// targets. Scoped declarations push onto `locals` and pop on scope
/// exit while `peak_local_count` retains the high-water mark so the
/// [`FrameLayout`] reserves enough slots for the whole function.
pub(super) struct LoweringContext<'a> {
    /// Identifiers of the function's regular (non-rest) parameters,
    /// in declaration order. `param_names[i]` is bound to register
    /// `i` (user-visible slot `i`, absolute slot `hidden_count + i`).
    param_names: Vec<&'a str>,
    /// Number of regular parameter slots in the frame, used to
    /// compute the next local slot index
    /// (`param_count + locals.len()`). Excludes the rest param —
    /// the rest binding lives in the locals region.
    param_count: u16,
    locals: Vec<LocalBinding<'a>>,
    /// High-water mark of `locals.len()`. The frame layout reserves
    /// this many slots so a binding that came in via a scoped path
    /// (e.g. `for (let i = 0; …)`) and was popped by
    /// [`restore_scope`](Self::restore_scope) still has its slot
    /// reserved for the duration of the function.
    peak_local_count: RegisterIndex,
    /// Temps currently in use (acquired but not yet released). Temps
    /// live in the user-visible register window after the local
    /// region; their indices start at `param_count + peak_local_count`
    /// and grow upward. `Cell` so `lower_call_expression` can
    /// acquire/release through a shared `&LoweringContext` borrow
    /// (every other expression-lowering helper takes `&` too).
    current_temp_count: Cell<RegisterIndex>,
    /// High-water mark of `current_temp_count`. Drives the
    /// `temporary_count` field on the `FrameLayout` so the frame
    /// reserves enough room for the deepest call-argument window
    /// the function reaches. `Cell` for the same reason as
    /// `current_temp_count`.
    peak_temp_count: Cell<RegisterIndex>,
    /// Names of every top-level `FunctionDeclaration` in the module,
    /// indexed by `FunctionIndex`. Used by `lower_call_expression`
    /// to translate a callee identifier into a `CallDirect` opcode.
    /// Ordered the same way the functions appear in
    /// `Module::functions`.
    function_names: &'a [&'a str],
    /// Next [`FeedbackSlot`] id to hand out. Incremented every time an
    /// arithmetic op is emitted with an attached feedback slot. The
    /// final count seeds the function's [`FeedbackTableLayout`].
    /// `Cell` so the expression-lowering helpers that take `&self`
    /// can still allocate a slot.
    next_feedback_slot: Cell<u16>,
    /// P1: Kind of each allocated feedback slot in emission order.
    /// Starts empty; every `allocate_*_feedback` call pushes its
    /// kind. `build_feedback_layout` reads this to construct a
    /// heterogeneous `FeedbackTableLayout` — without it, every
    /// slot would be `Arithmetic` and the dispatcher's property
    /// inline-cache probe would read a fresh `ArithmeticFeedback`
    /// on every lookup (free miss every iteration).
    feedback_slot_kinds: RefCell<Vec<FeedbackKind>>,
    /// Innermost-loop-first stack of [`LoopLabels`] frames. Pushed on
    /// loop entry by `lower_while_statement` / `lower_for_statement`
    /// and popped on loop exit. `break` reads `break_label` from the
    /// top frame; `continue` reads `continue_label`. Nested loops
    /// stack; the outermost sits at index 0, so `.last()` resolves
    /// the innermost.
    ///
    /// `RefCell` (not `Cell`) because `Label` is `Copy` but the stack
    /// type itself isn't. `enter_loop` / `exit_loop` are the only
    /// mutators.
    loop_labels: RefCell<Vec<LoopLabels>>,
    /// Innermost-finally-last stack of normal-path finalizer entry
    /// labels. `return` / `break` / `continue` queue the outer
    /// entries here so `ResumeAbrupt` can unwind through each
    /// `finally` block before resuming the original completion.
    finally_frames: RefCell<Vec<FinallyFrame>>,
    /// Stack of short-circuit labels for the currently-open optional
    /// chain expressions (§13.3.9). `lower_chain_expression` pushes
    /// a fresh label before lowering the chain's element tree and
    /// pops it afterwards. When a member / call with `optional:
    /// true` is reached inside `lower_static_member_read`-style
    /// helpers, the helper emits a nullish-check jump to the
    /// innermost label. `Cell::get`-style peeking is enough (only
    /// the innermost label matters); reads through `.last()`.
    optional_chain_short_circuit: RefCell<Vec<Label>>,
    /// §14.13 Labelled statements — when a `LabeledStatement`
    /// immediately wraps an iteration statement (`for` / `while` /
    /// `do-while` / `for-of` / `for-in`) or a `switch`, the label
    /// is stashed here before the body is lowered. The loop /
    /// switch lowerer consumes the label when it pushes its
    /// `LoopLabels` frame so `break labelName` / `continue
    /// labelName` can find the matching frame. Cleared after every
    /// push — nested labels on a single statement aren't a thing,
    /// so a single slot is enough.
    pending_loop_label: std::cell::RefCell<Option<std::rc::Rc<str>>>,
    /// Stack of `locals.len()` snapshots marking the start of each
    /// currently-open lexical scope (M12). Pushed by
    /// [`snapshot_scope`](Self::snapshot_scope) and popped by
    /// [`restore_scope`](Self::restore_scope).
    ///
    /// The innermost scope starts at
    /// `scope_starts.last().unwrap_or(&0)`. `allocate_local` checks
    /// for duplicates only within that window, so `let x` inside a
    /// nested block can legally shadow an outer `let x`.
    ///
    /// Function top-scope has `scope_starts` empty (index 0 is
    /// implicit). The parameter name still participates in the
    /// top-scope duplicate check — function parameters and
    /// function-scope `let`/`const` live in the same lexical
    /// environment per the ES spec.
    scope_starts: RefCell<Vec<usize>>,
    /// Deduplicated property-name interner (M14). Grows when the
    /// compiler emits `LdaGlobal` / `StaGlobal` for a previously-
    /// unseen identifier, with the interned index used as the
    /// `Idx` operand. Handed to [`PropertyNameTable::new`] at
    /// function finalisation so the dispatcher can resolve the name
    /// back to a string at runtime.
    property_names: RefCell<Vec<String>>,
    /// Deduplicated float-constant interner (M14). Currently only
    /// used for materialising `Infinity` / `-Infinity` (int32
    /// literals still flow through `LdaSmi`). Handed to
    /// [`FloatTable::new`](crate::float::FloatTable::new) at
    /// function finalisation.
    float_constants: RefCell<Vec<f64>>,
    /// Deduplicated string-literal interner (M15). Grows when the
    /// compiler emits `LdaConstStr` for a string literal. Handed to
    /// [`StringTable::new`](crate::string::StringTable::new) at
    /// function finalisation so the dispatcher can resolve the
    /// `Idx` operand back to a `JsString` at runtime.
    string_literals: RefCell<Vec<String>>,
    /// M36: deduplicated BigInt-literal interner. Each entry is a
    /// decimal-string representation of an arbitrary-precision
    /// integer (the source suffix `n` stripped). Handed to
    /// [`BigIntTable::new`](crate::bigint::BigIntTable::new) at
    /// function finalisation.
    bigint_constants: RefCell<Vec<String>>,
    /// M36: RegExp-literal table. Each entry is a `(pattern, flags)`
    /// pair; no dedup (two equal-looking literals still produce
    /// independent RegExp objects per §22.2.1.5). Handed to
    /// [`RegExpTable::new`](crate::regexp::RegExpTable::new) at
    /// function finalisation.
    regexp_literals: RefCell<Vec<(String, String)>>,
    /// Pending exception-handler records (M21). Each entry pairs a
    /// try-block's `(try_start_label, try_end_label)` range with the
    /// `handler_label` the interpreter should jump to on an
    /// in-range throw. PCs are resolved out of the builder's label
    /// table after all three labels are bound, just before the
    /// `ExceptionTable` is constructed in
    /// [`lower_function_body`].
    pending_handlers: RefCell<Vec<PendingExceptionHandler>>,
    /// Pending ClosureTemplate entries (M25). Each
    /// `CreateClosure <idx>, <flags>` site produces one entry
    /// keyed by its byte PC. At function finalisation the entries
    /// are materialised into a sparse `ClosureTable` indexed by
    /// PC (empty slots between closure-creation sites stay `None`).
    pending_closure_templates: RefCell<Vec<PendingClosureTemplate>>,
    /// Shared, growing module-level function list. Inner
    /// `FunctionExpression`s lowered during this context's body
    /// emission push their produced `VmFunction` here and learn
    /// their `FunctionIndex` from the post-push length. Every
    /// context created during a single `compile()` call shares
    /// the same `Rc<RefCell<…>>`.
    module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    /// Enclosing function's context for closure capture lookups.
    /// `None` on top-level functions. Stored as a raw-pointer
    /// reference via `Option<&'a LoweringContext<'a>>` — the
    /// parent outlives every descendant because children are
    /// constructed inside the parent's body-lowering call.
    parent: Option<&'a LoweringContext<'a>>,
    /// Captured outer bindings, in upvalue-index order. Each
    /// entry corresponds to one `LdaUpvalue` / `StaUpvalue`
    /// operand inside this function and one `CaptureDescriptor`
    /// the parent's `ClosureTemplate` carries. Name is owned
    /// (`String`) instead of `&'a str` so the field doesn't
    /// contribute to `LoweringContext<'a>`'s invariance —
    /// `Option<&'a LoweringContext<'a>>` for the `parent` field
    /// would otherwise propagate invariance through every
    /// function signature that touches `ctx`.
    captures: RefCell<Vec<CaptureEntry>>,
    /// M28: super-expression eligibility for the current function.
    /// `None` for ordinary functions; `Some` only when this
    /// `LoweringContext` is compiling a class method, getter,
    /// setter, or constructor.
    ///
    /// Arrow functions inside a class method do NOT inherit this
    /// flag in M28 — the source compiler rejects `super` inside
    /// arrows with `super_in_arrow`. The runtime's
    /// `[[HomeObject]]` inheritance on arrow closures would cover
    /// the runtime side, but without the compile-time gating the
    /// compiler cannot validate out-of-class `super` uses.
    class_super_binding: Option<ClassSuperBinding>,
    /// M29: names declared in the immediately enclosing class
    /// body's private-name scope (without the leading `#`). Shared
    /// by reference across all methods / accessors / field
    /// initializers of one class body so a single `#x in obj`
    /// check inside any of them can validate the name.
    ///
    /// Empty for non-class contexts. For M29 scope: private-name
    /// resolution does NOT walk parent classes, so nested-class
    /// access to outer `#x` is rejected (`undeclared_private_name`).
    class_private_names: std::rc::Rc<[String]>,
    /// M35: names that live on the runtime global object by the time
    /// this function executes — both `import`-bound locals (set by
    /// `populate_import_globals`) and top-level `export`ed
    /// declarations (installed by the synthesised module-init
    /// function). An identifier reference that doesn't resolve to a
    /// local / parameter / upvalue / top-level `FunctionDeclaration`
    /// falls through to `LdaGlobal` when its name is in this list,
    /// instead of failing with `unbound_identifier`.
    ///
    /// Shared by `Rc<RefCell<…>>` across every nested
    /// `LoweringContext` in the same compilation so a nested
    /// closure can resolve an imported binding declared at module
    /// scope without plumbing.
    module_globals: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    /// D2: index over the original source text. Shared across
    /// every nested context in the same compilation; `None` for
    /// synthesised functions (module-init, static-block) that
    /// have no user-visible source.
    source_index: Option<std::rc::Rc<crate::source_map::SourceTextIndex>>,
    /// D2: per-function pending source-map entries. Drained by
    /// `take_source_map` into a finalised [`crate::source_map::SourceMap`]
    /// when the function body finishes lowering.
    pending_source_map: RefCell<Vec<crate::source_map::SourceMapEntry>>,
}

/// §13.3.7 / §15.7.14 — per-function metadata describing which
/// forms of `super` are syntactically valid inside the function's
/// body.
#[derive(Debug, Clone, Copy)]
struct ClassSuperBinding {
    /// `super.x` / `super[k]` / `super.m(args)` — allowed for any
    /// class method / getter / setter / constructor. Gated by the
    /// presence of `[[HomeObject]]` on the active closure.
    allow_super_property: bool,
    /// `super(args)` — allowed only in derived-class constructors.
    allow_super_call: bool,
}

/// M29: one `MethodDefinition` (method or accessor) from a
/// `ClassBody`. Accessors (`get`/`set`) land in the same bucket
/// as regular methods — the installer branches on `kind` when
/// choosing between `StaNamedProperty` and the accessor opcodes.
/// M29.5 adds the `is_private` bit so `#m() {}` / `get #p() {}`
/// get routed through `PushPrivate*` / `DefinePrivate*` instead.
struct ClassMethod<'a> {
    name: String,
    is_static: bool,
    is_private: bool,
    kind: MethodDefinitionKind,
    func: &'a Function<'a>,
}

/// M29: one class field declaration. Represents both public
/// (`x = expr;`) and private (`#x = expr;`) fields, instance and
/// static. The initializer lives on the AST and is lowered inside
/// the class body's field-initializer closure (or inline for
/// statics).
struct ClassField<'a> {
    /// Field name without the leading `#` prefix for private fields.
    name: String,
    /// `true` when the declaration used `#` (private element).
    is_private: bool,
    /// Optional initializer expression. Absent initializers
    /// default to `undefined` per §15.7.14.
    initializer: Option<&'a Expression<'a>>,
    span: Span,
}

/// M29.5: per-private-name declaration bookkeeping for
/// §15.7.11's early-error check. `Getter`/`Setter` merge into
/// `GetterSetter` when both halves are seen; every other
/// collision is a duplicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivateDecl {
    Field,
    Method,
    Getter,
    Setter,
    GetterSetter,
}

/// Validates a new private-name declaration against the running
/// `private_decls` list. Returns Ok when the declaration is
/// either fresh or the complementary half of an existing
/// getter/setter pair; returns `duplicate_private_name` otherwise.
fn record_private_decl(
    decls: &mut Vec<(String, PrivateDecl)>,
    name: &str,
    new_kind: PrivateDecl,
    span: Span,
) -> Result<(), SourceLoweringError> {
    if let Some(slot) = decls.iter_mut().find(|(n, _)| n == name) {
        let merged = match (slot.1, new_kind) {
            (PrivateDecl::Getter, PrivateDecl::Setter)
            | (PrivateDecl::Setter, PrivateDecl::Getter) => PrivateDecl::GetterSetter,
            _ => {
                return Err(SourceLoweringError::unsupported(
                    "duplicate_private_name",
                    span,
                ));
            }
        };
        slot.1 = merged;
    } else {
        decls.push((name.to_owned(), new_kind));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct CaptureEntry {
    name: String,
    descriptor: crate::closure::CaptureDescriptor,
}

/// Pre-resolution form of an `ExceptionHandler`. All three fields
/// are labels allocated from the current function's
/// `BytecodeBuilder`; they resolve to PCs at function finalisation.
#[derive(Debug, Clone, Copy)]
struct PendingExceptionHandler {
    try_start: Label,
    try_end: Label,
    handler: Label,
}

/// Pre-resolution form of a `ClosureTemplate`. Stores the PC at
/// which the `CreateClosure` opcode will be emitted so the
/// finaliser can build a PC-indexed sparse `ClosureTable`.
#[derive(Debug, Clone)]
struct PendingClosureTemplate {
    pc: u32,
    template: crate::closure::ClosureTemplate,
}

/// `break` / `continue` jump targets for one enclosing control
/// frame. `break_label` is bound to the instruction immediately
/// after the loop or switch; `continue_label` is the re-entry
/// point — for `while`, the loop header (re-evaluates the
/// condition); for `for`, the update clause (evaluates the update,
/// then jumps back to the header); for `switch`, `None` since
/// `continue` inside a switch body walks past the switch to the
/// enclosing loop (§14.11).
#[derive(Debug, Clone)]
struct LoopLabels {
    break_label: Label,
    continue_label: Option<Label>,
    /// §14.13 Labelled statement — name of the immediately-enclosing
    /// `LabeledStatement` if any, shared via `Rc<str>` so the
    /// break/continue lowerers can compare against label identifiers
    /// without reallocating per frame. `None` for plain loops.
    label: Option<std::rc::Rc<str>>,
}

#[derive(Debug, Clone)]
struct FinallyFrame {
    normal_entry: Label,
    internal_jumps: Vec<Label>,
}

/// Snapshot of [`LoweringContext::locals`] length, returned by
/// [`LoweringContext::snapshot_scope`] and consumed by
/// [`LoweringContext::restore_scope`]. Used to give scoped
/// declarations (currently only `for` init `let`s) a real lexical
/// lifetime instead of leaking them to the surrounding function
/// scope. The peak local count is preserved across snapshot/restore.
struct ScopeSnapshot {
    len: usize,
}

impl<'a> LoweringContext<'a> {
    fn new(
        layout: &ParamsLayout<'a>,
        function_names: &'a [&'a str],
        module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
    ) -> Self {
        Self::with_parent(layout, function_names, module_functions, None, None, None)
    }

    fn with_parent(
        layout: &ParamsLayout<'a>,
        function_names: &'a [&'a str],
        module_functions: std::rc::Rc<std::cell::RefCell<Vec<VmFunction>>>,
        parent: Option<&'a LoweringContext<'a>>,
        class_super_binding: Option<ClassSuperBinding>,
        class_private_names: Option<std::rc::Rc<[String]>>,
    ) -> Self {
        let param_names = layout.names.clone();
        let param_count = RegisterIndex::try_from(param_names.len()).unwrap_or(u16::MAX);
        // M35: inherit the module-globals table from the parent if we
        // have one (nested closures and inner functions live in the
        // same module). Top-level contexts pick up the table from the
        // `MODULE_GLOBALS_OVERRIDE` thread-local that `lower_program`
        // set before dispatching each top-level declaration; when
        // that override is absent (non-ESM scripts, test helpers
        // constructing a context directly) we fall back to a fresh
        // empty list.
        let module_globals = parent
            .map(|p| std::rc::Rc::clone(&p.module_globals))
            .or_else(|| {
                MODULE_GLOBALS_OVERRIDE.with(|slot| slot.borrow().as_ref().map(std::rc::Rc::clone))
            })
            .unwrap_or_else(|| std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
        // D2: each nested context inherits the source-text index
        // from its parent; top-level contexts pull it from the
        // thread-local override set by `ModuleCompiler::compile`.
        let source_index = parent
            .and_then(|p| p.source_index.as_ref().map(std::rc::Rc::clone))
            .or_else(|| {
                SOURCE_INDEX_OVERRIDE.with(|slot| slot.borrow().as_ref().map(std::rc::Rc::clone))
            });
        Self {
            param_names,
            param_count,
            locals: Vec::new(),
            peak_local_count: 0,
            current_temp_count: Cell::new(0),
            peak_temp_count: Cell::new(0),
            function_names,
            next_feedback_slot: Cell::new(0),
            feedback_slot_kinds: RefCell::new(Vec::new()),
            loop_labels: RefCell::new(Vec::new()),
            finally_frames: RefCell::new(Vec::new()),
            optional_chain_short_circuit: RefCell::new(Vec::new()),
            pending_loop_label: std::cell::RefCell::new(None),
            scope_starts: RefCell::new(Vec::new()),
            property_names: RefCell::new(Vec::new()),
            float_constants: RefCell::new(Vec::new()),
            string_literals: RefCell::new(Vec::new()),
            bigint_constants: RefCell::new(Vec::new()),
            regexp_literals: RefCell::new(Vec::new()),
            pending_handlers: RefCell::new(Vec::new()),
            pending_closure_templates: RefCell::new(Vec::new()),
            module_functions,
            parent,
            captures: RefCell::new(Vec::new()),
            class_super_binding,
            class_private_names: class_private_names
                .unwrap_or_else(|| std::rc::Rc::<[String]>::from([])),
            module_globals,
            source_index,
            pending_source_map: RefCell::new(Vec::new()),
        }
    }

    /// D2: record a `(pc → source location)` entry, resolving the
    /// oxc byte offset through the shared `SourceTextIndex`. No-op
    /// when the context has no source index (synthesised
    /// functions, test harnesses constructing a bare context).
    fn record_source_location(&self, pc: u32, byte_offset: u32) {
        let Some(idx) = &self.source_index else {
            return;
        };
        let location = idx.resolve(byte_offset);
        self.pending_source_map
            .borrow_mut()
            .push(crate::source_map::SourceMapEntry::new(pc, location));
    }

    /// D2: drains the accumulated source-map entries, dedup-
    /// adjacent-same-PC entries, and returns a finalised table
    /// sorted by PC (emission order is already PC-ascending).
    fn take_source_map(&self) -> crate::source_map::SourceMap {
        let entries = std::mem::take(&mut *self.pending_source_map.borrow_mut());
        crate::source_map::SourceMap::new(entries)
    }

    /// Returns `true` if `name` is declared as a module-level global
    /// for the current compilation (see [`module_globals`]). Used by
    /// identifier-reference and call lowering to route resolution
    /// through `LdaGlobal` instead of rejecting the name.
    fn is_module_global(&self, name: &str) -> bool {
        self.module_globals.borrow().iter().any(|n| n == name)
    }

    /// Register a ClosureTemplate at the given `pc`. The PC is the
    /// byte offset of a `CreateClosure` opcode just emitted by the
    /// body lowerer; the finaliser builds a sparse
    /// `ClosureTable` indexed by PC so the dispatcher can look up
    /// the template per opcode.
    fn record_closure_template(&self, pc: u32, template: crate::closure::ClosureTemplate) {
        self.pending_closure_templates
            .borrow_mut()
            .push(PendingClosureTemplate { pc, template });
    }

    /// Finalise pending closure templates into a sparse
    /// `ClosureTable` sized to the function's total bytecode
    /// length. Empty PC slots stay `None`; closure-creation sites
    /// get their registered `ClosureTemplate`.
    fn take_closure_table(&self, bytecode_len: u32) -> crate::closure::ClosureTable {
        let drained = std::mem::take(&mut *self.pending_closure_templates.borrow_mut());
        if drained.is_empty() {
            return crate::closure::ClosureTable::empty();
        }
        let mut templates: Vec<Option<crate::closure::ClosureTemplate>> =
            vec![None; bytecode_len as usize];
        for entry in drained {
            let idx = entry.pc as usize;
            if idx < templates.len() {
                templates[idx] = Some(entry.template);
            }
        }
        crate::closure::ClosureTable::new(templates)
    }

    /// Register a `try { … } catch/finally { … }` protected range
    /// for emission into the function's `ExceptionTable` after
    /// labels resolve.
    fn record_exception_handler(&self, try_start: Label, try_end: Label, handler: Label) {
        self.pending_handlers
            .borrow_mut()
            .push(PendingExceptionHandler {
                try_start,
                try_end,
                handler,
            });
    }

    /// Drain the pending-handler list and resolve each entry into a
    /// concrete [`crate::exception::ExceptionHandler`]. Returns an
    /// error if any label ended up unbound — that's an internal bug
    /// in the lowering (every registered handler must have all three
    /// labels bound before this is called).
    fn take_exception_handlers(
        &self,
        builder: &BytecodeBuilder,
    ) -> Result<Vec<crate::exception::ExceptionHandler>, SourceLoweringError> {
        let drained = std::mem::take(&mut *self.pending_handlers.borrow_mut());
        let mut resolved = Vec::with_capacity(drained.len());
        for h in drained {
            let try_start = builder.label_pc(h.try_start).ok_or_else(|| {
                SourceLoweringError::Internal("exception handler try_start unbound".into())
            })?;
            let try_end = builder.label_pc(h.try_end).ok_or_else(|| {
                SourceLoweringError::Internal("exception handler try_end unbound".into())
            })?;
            let handler_pc = builder.label_pc(h.handler).ok_or_else(|| {
                SourceLoweringError::Internal("exception handler handler unbound".into())
            })?;
            resolved.push(crate::exception::ExceptionHandler::new(
                try_start, try_end, handler_pc,
            ));
        }
        Ok(resolved)
    }

    /// Intern a property name into the function's side table,
    /// returning its index for use as an `Idx` operand (e.g., on
    /// `LdaGlobal`). Dedup is O(N) on an already-small table.
    fn intern_property_name(&self, name: &str) -> Result<u32, SourceLoweringError> {
        let mut tbl = self.property_names.borrow_mut();
        if let Some(pos) = tbl.iter().position(|n| n == name) {
            return Ok(pos as u32);
        }
        let idx = u32::try_from(tbl.len())
            .map_err(|_| SourceLoweringError::Internal("property name table overflow".into()))?;
        tbl.push(name.to_owned());
        Ok(idx)
    }

    /// Intern a float constant into the function's side table,
    /// returning its index. Uses `to_bits` for equality so
    /// `Infinity` and `NaN` dedup correctly despite NaN's pathological
    /// `==` behaviour.
    fn intern_float_constant(&self, value: f64) -> Result<u32, SourceLoweringError> {
        let mut tbl = self.float_constants.borrow_mut();
        let bits = value.to_bits();
        if let Some(pos) = tbl.iter().position(|v| v.to_bits() == bits) {
            return Ok(pos as u32);
        }
        let idx = u32::try_from(tbl.len())
            .map_err(|_| SourceLoweringError::Internal("float constant table overflow".into()))?;
        tbl.push(value);
        Ok(idx)
    }

    /// Finalise the property-name interner into an immutable table.
    fn take_property_names(&self) -> crate::property::PropertyNameTable {
        crate::property::PropertyNameTable::new(self.property_names.borrow().clone())
    }

    /// Finalise the float-constant interner into an immutable table.
    fn take_float_constants(&self) -> crate::float::FloatTable {
        crate::float::FloatTable::new(self.float_constants.borrow().clone())
    }

    /// Intern a string literal into the function's side table,
    /// returning its index for use as an `Idx` operand on
    /// `LdaConstStr`. Dedup is O(N) on an already-small table.
    fn intern_string_literal(&self, value: &str) -> Result<u32, SourceLoweringError> {
        let mut tbl = self.string_literals.borrow_mut();
        if let Some(pos) = tbl.iter().position(|n| n == value) {
            return Ok(pos as u32);
        }
        let idx = u32::try_from(tbl.len())
            .map_err(|_| SourceLoweringError::Internal("string literal table overflow".into()))?;
        tbl.push(value.to_owned());
        Ok(idx)
    }

    /// Finalise the string-literal interner into an immutable table.
    fn take_string_literals(&self) -> crate::string::StringTable {
        crate::string::StringTable::new(self.string_literals.borrow().clone())
    }

    /// Intern a BigInt literal's decimal-string representation
    /// (the source suffix `n` stripped) into the function's
    /// BigInt-constant side table. Dedup is O(N) on an
    /// already-small table.
    fn intern_bigint_literal(&self, decimal: &str) -> Result<u32, SourceLoweringError> {
        let mut tbl = self.bigint_constants.borrow_mut();
        if let Some(pos) = tbl.iter().position(|n| n == decimal) {
            return Ok(pos as u32);
        }
        let idx = u32::try_from(tbl.len())
            .map_err(|_| SourceLoweringError::Internal("bigint constant table overflow".into()))?;
        tbl.push(decimal.to_owned());
        Ok(idx)
    }

    /// Finalise the BigInt-constant interner into an immutable table.
    fn take_bigint_constants(&self) -> crate::bigint::BigIntTable {
        let values: Vec<Box<str>> = self
            .bigint_constants
            .borrow()
            .iter()
            .map(|s| s.clone().into_boxed_str())
            .collect();
        crate::bigint::BigIntTable::new(values)
    }

    /// Register a `(pattern, flags)` RegExp entry, returning its
    /// index for the `CreateRegExp` opcode's `Idx` operand. No
    /// dedup — §22.2.1.5 specifies a fresh RegExp object per
    /// literal evaluation.
    fn push_regexp_literal(&self, pattern: &str, flags: &str) -> Result<u32, SourceLoweringError> {
        let mut tbl = self.regexp_literals.borrow_mut();
        let idx = u32::try_from(tbl.len())
            .map_err(|_| SourceLoweringError::Internal("regexp literal table overflow".into()))?;
        tbl.push((pattern.to_owned(), flags.to_owned()));
        Ok(idx)
    }

    /// Finalise the RegExp-literal table into its immutable form.
    fn take_regexp_literals(&self) -> crate::regexp::RegExpTable {
        let entries: Vec<(Box<str>, Box<str>)> = self
            .regexp_literals
            .borrow()
            .iter()
            .map(|(p, f)| (p.clone().into_boxed_str(), f.clone().into_boxed_str()))
            .collect();
        crate::regexp::RegExpTable::new(entries)
    }

    /// Push a fresh [`LoopLabels`] frame onto the stack. Paired 1:1
    /// with [`Self::exit_loop`] — `lower_while_statement` and
    /// `lower_for_statement` always pop before returning to their
    /// caller.
    fn enter_loop(&self, labels: LoopLabels) {
        self.loop_labels.borrow_mut().push(labels);
    }

    fn enter_finally_frame(&self, frame: FinallyFrame) {
        self.finally_frames.borrow_mut().push(frame);
    }

    fn exit_finally_frame(&self) {
        let popped = self.finally_frames.borrow_mut().pop();
        debug_assert!(
            popped.is_some(),
            "exit_finally_frame called without enter_finally_frame",
        );
    }

    fn active_finally_targets_for_jump(&self, target: Option<Label>) -> Vec<Label> {
        let frames = self.finally_frames.borrow();
        let start = target
            .and_then(|target| {
                frames
                    .iter()
                    .rposition(|frame| frame.internal_jumps.contains(&target))
            })
            .map_or(0, |idx| idx + 1);
        frames[start..]
            .iter()
            .map(|frame| frame.normal_entry)
            .collect()
    }

    /// §14.13 — stash a label so the immediately-following iteration
    /// statement picks it up when pushing its `LoopLabels` frame.
    fn set_pending_loop_label(&self, name: std::rc::Rc<str>) {
        *self.pending_loop_label.borrow_mut() = Some(name);
    }

    /// Drain the pending label (if any) and return it. Called by
    /// every loop / switch lowerer at `enter_loop` time.
    fn take_pending_loop_label(&self) -> Option<std::rc::Rc<str>> {
        self.pending_loop_label.borrow_mut().take()
    }

    /// Walk the loop-labels stack from innermost out and return the
    /// break label of the frame whose `label` matches `name`. Used
    /// by `break labelName` — spec §14.12 returns
    /// `undeclared_label` when no frame matches.
    fn find_break_label_by_name(&self, name: &str) -> Option<Label> {
        self.loop_labels
            .borrow()
            .iter()
            .rev()
            .find(|f| f.label.as_deref() == Some(name))
            .map(|f| f.break_label)
    }

    /// Walk the loop-labels stack from innermost out and return the
    /// continue label of the first frame whose `label` matches
    /// `name` AND that has a continue target. `continue labelName`
    /// is valid only for iteration statements (§14.11 / §14.13) —
    /// a labelled `switch` or labelled block doesn't accept
    /// `continue`.
    fn find_continue_label_by_name(&self, name: &str) -> Option<Label> {
        self.loop_labels
            .borrow()
            .iter()
            .rev()
            .find(|f| f.label.as_deref() == Some(name))
            .and_then(|f| f.continue_label)
    }

    /// Pop the most-recent [`LoopLabels`] frame. Panics in
    /// `debug_assertions` if the stack is empty, because that would
    /// mean an unbalanced `enter_loop` / `exit_loop` pair — a
    /// programmer error the emitter wants to catch eagerly.
    fn exit_loop(&self) {
        let popped = self.loop_labels.borrow_mut().pop();
        debug_assert!(popped.is_some(), "exit_loop called without enter_loop");
    }

    /// Returns the innermost loop's break target, if any. `None`
    /// means we're currently lowering code outside every loop — the
    /// statement handlers use this to surface `break_outside_loop` /
    /// `continue_outside_loop` errors.
    fn innermost_break_label(&self) -> Option<Label> {
        self.loop_labels.borrow().last().map(|f| f.break_label)
    }

    /// Push a short-circuit label for an optional chain. Paired 1:1
    /// with [`Self::exit_optional_chain`]. The label is bound after
    /// the chain's last access so that any `?.` along the way can
    /// nullish-short-circuit to it.
    fn enter_optional_chain(&self, short_circuit: Label) {
        self.optional_chain_short_circuit
            .borrow_mut()
            .push(short_circuit);
    }

    fn exit_optional_chain(&self) {
        let popped = self.optional_chain_short_circuit.borrow_mut().pop();
        debug_assert!(
            popped.is_some(),
            "exit_optional_chain called without enter_optional_chain",
        );
    }

    /// Returns the innermost optional-chain short-circuit label, if
    /// any. `Some` only while we're actively lowering inside a
    /// [`ChainExpression`]; property/member/call lowerers peek at
    /// this to know whether `expr.optional` should trigger a
    /// short-circuit jump (inside a chain) or stay rejected
    /// (outside — which the parser doesn't actually produce, but
    /// the defensive check stays as a guard).
    fn optional_chain_short_circuit(&self) -> Option<Label> {
        self.optional_chain_short_circuit.borrow().last().copied()
    }

    /// Returns the innermost enclosing `continue`-capable frame's
    /// jump target. Walks past switch frames (whose
    /// `continue_label` is `None`) to find a real loop —
    /// `continue` inside `switch` targets the enclosing loop per
    /// §14.11, not the switch itself.
    fn innermost_continue_label(&self) -> Option<Label> {
        self.loop_labels
            .borrow()
            .iter()
            .rev()
            .find_map(|f| f.continue_label)
    }

    /// Allocates a fresh arithmetic-feedback slot id, returning the
    /// [`FeedbackSlot`] the caller should attach to its freshly-emitted
    /// instruction via
    /// [`BytecodeBuilder::attach_feedback`](crate::bytecode::BytecodeBuilder::attach_feedback).
    ///
    /// Slot ids are sequential (`0`, `1`, …); the final count drives the
    /// size of the function's [`FeedbackTableLayout`]. Every allocated
    /// slot is assumed [`FeedbackKind::Arithmetic`] — the M_JIT_C.2 side
    /// table only tracks int32-trust feedback and intentionally does not
    /// populate Comparison/Branch/Property/Call slots.
    ///
    /// Panics in `debug_assertions` when the counter overflows `u16`;
    /// release builds saturate and the surplus ops simply share the
    /// last slot (correctness-preserving: the analyzer's trust map
    /// still reflects the worst of the overlapping observations).
    fn allocate_arithmetic_feedback(&self) -> FeedbackSlot {
        let id = self.next_feedback_slot.get();
        debug_assert!(
            id < u16::MAX,
            "feedback slot counter overflow — pathological function > 65 535 arithmetic ops",
        );
        self.next_feedback_slot.set(id.saturating_add(1));
        self.feedback_slot_kinds
            .borrow_mut()
            .push(FeedbackKind::Arithmetic);
        FeedbackSlot(id)
    }

    /// P1: allocate a [`FeedbackKind::Property`] slot. Attached to
    /// the PC of a `LdaNamedProperty` (or similar) so the
    /// dispatcher can probe the cached `(shape_id, slot_index)`
    /// pairs and short-circuit the prototype-chain walk when the
    /// object's shape still matches one of the observed shapes
    /// (monomorphic or polymorphic up to 4 shapes, then
    /// megamorphic fallback).
    fn allocate_property_feedback(&self) -> FeedbackSlot {
        let id = self.next_feedback_slot.get();
        debug_assert!(
            id < u16::MAX,
            "feedback slot counter overflow — pathological function > 65 535 feedback ops",
        );
        self.next_feedback_slot.set(id.saturating_add(1));
        self.feedback_slot_kinds
            .borrow_mut()
            .push(FeedbackKind::Property);
        FeedbackSlot(id)
    }

    /// Current count of allocated arithmetic-feedback slots. Consumed
    /// by [`lower_function_body`] to build the function's
    /// [`FeedbackTableLayout`].
    fn feedback_slot_count(&self) -> u16 {
        self.next_feedback_slot.get()
    }

    /// P1: drains the accumulated feedback-kind vector, returning
    /// it so the function finaliser can shape the
    /// `FeedbackTableLayout` heterogeneously.
    fn take_feedback_slot_kinds(&self) -> Vec<FeedbackKind> {
        std::mem::take(&mut *self.feedback_slot_kinds.borrow_mut())
    }

    /// Number of `let`/`const` slots reserved by the frame layout —
    /// the high-water mark of `locals.len()`, **not** the current
    /// length. Bindings popped by [`restore_scope`] still occupy
    /// their slots until the function returns, so the FrameLayout
    /// must size for the peak.
    fn local_count(&self) -> RegisterIndex {
        self.peak_local_count
    }

    /// Number of `temporary` slots reserved by the frame layout —
    /// the high-water mark of `current_temp_count`. Temps live in
    /// the user-visible register window after the local region and
    /// are used by `lower_call_expression` to materialize a
    /// contiguous arg buffer for `CallDirect`.
    fn temp_count(&self) -> RegisterIndex {
        self.peak_temp_count.get()
    }

    /// Acquires `count` consecutive temp slots and returns the
    /// user-visible register index of the first one. Caller must
    /// call [`release_temps`](Self::release_temps) with the same
    /// `count` once it's done with the slots — typically in a
    /// LIFO pattern, mirroring nested call expressions. Takes
    /// `&self` so it can be called from the `&LoweringContext`
    /// expression-lowering paths; mutation lives behind `Cell` for
    /// the temp counters.
    fn acquire_temps(&self, count: RegisterIndex) -> Result<u16, SourceLoweringError> {
        let local_room = self
            .param_count
            .checked_add(self.peak_local_count)
            .ok_or_else(|| {
                SourceLoweringError::Internal("temp base overflow (params + locals)".into())
            })?;
        let in_use = self.current_temp_count.get();
        let base = local_room.checked_add(in_use).ok_or_else(|| {
            SourceLoweringError::Internal("temp base overflow (in-use temps)".into())
        })?;
        let new_used = in_use
            .checked_add(count)
            .ok_or_else(|| SourceLoweringError::Internal("temp count overflow".into()))?;
        if new_used > self.peak_temp_count.get() {
            self.peak_temp_count.set(new_used);
        }
        self.current_temp_count.set(new_used);
        Ok(base)
    }

    /// Releases `count` temp slots — the matching pair of
    /// [`acquire_temps`](Self::acquire_temps). Slots are reusable by
    /// later calls but stay reserved by the frame layout's
    /// `temporary_count` (which tracks the peak, not the live count).
    fn release_temps(&self, count: RegisterIndex) {
        let in_use = self.current_temp_count.get();
        debug_assert!(
            in_use >= count,
            "release_temps under-flow: have {in_use}, releasing {count}",
        );
        self.current_temp_count.set(in_use.saturating_sub(count));
    }

    /// Resolves a top-level function name to its `FunctionIndex`.
    /// Used by [`lower_call_expression`] to translate `f(args)` into
    /// `CallDirect(f_idx, …)`. Returns `None` for unknown names —
    /// the caller surfaces a `SourceLoweringError::Unsupported`
    /// (typically with the `unbound_function` tag).
    fn resolve_function(&self, name: &str) -> Option<FunctionIndex> {
        self.function_names
            .iter()
            .position(|&n| n == name)
            .and_then(|idx| u32::try_from(idx).ok())
            .map(FunctionIndex)
    }

    /// Snapshots the current scope so a later [`restore_scope`] can
    /// pop bindings that came in between the two calls. Used by
    /// [`lower_for_statement`] to scope the for-init `let`/`const`
    /// to the loop, and by [`lower_block_statement`] (M12) to scope
    /// `let`/`const` inside a nested `{ ... }` to the block.
    ///
    /// Also pushes the current `locals.len()` onto `scope_starts` so
    /// [`allocate_local`](Self::allocate_local) can distinguish
    /// duplicate bindings in the SAME scope (rejected) from legal
    /// shadowing of outer-scope names.
    fn snapshot_scope(&self) -> ScopeSnapshot {
        let len = self.locals.len();
        self.scope_starts.borrow_mut().push(len);
        ScopeSnapshot { len }
    }

    /// Pops every binding allocated since the matching
    /// [`snapshot_scope`]. Slots stay reserved (via
    /// [`peak_local_count`](Self::peak_local_count)) so bindings
    /// allocated later don't collide with the popped ones'
    /// addresses.
    ///
    /// Also pops the matching `scope_starts` entry so subsequent
    /// `allocate_local` duplicate checks see the outer scope.
    fn restore_scope(&mut self, snapshot: ScopeSnapshot) {
        debug_assert!(
            snapshot.len <= self.locals.len(),
            "scope snapshot length must not grow",
        );
        let popped = self.scope_starts.borrow_mut().pop();
        debug_assert_eq!(
            popped,
            Some(snapshot.len),
            "scope_starts stack out of sync with scope snapshot",
        );
        self.locals.truncate(snapshot.len);
    }

    /// Allocates the next local slot for `name`. The new binding
    /// starts as **not yet initialized** so reads inside its own
    /// initializer surface as `tdz_self_reference`. Caller must call
    /// [`mark_initialized`](Self::mark_initialized) after emitting the
    /// post-init `Star r_slot`. `is_const` is captured from the
    /// declaration kind so [`lower_assignment_expression`] can reject
    /// writes to const bindings.
    ///
    /// The duplicate check (M12) operates on the innermost open
    /// scope only — a nested `let x` legally shadows an outer
    /// `let x` or an enclosing-function's `let x`. The function's
    /// parameter name participates in the top-scope check because
    /// parameters and function-scope `let`/`const` live in the same
    /// lexical environment per ES spec.
    ///
    /// Rejects:
    /// - duplicate name in the same scope (another local / the
    ///   parameter at top scope) →
    ///   `Unsupported { construct: "duplicate_binding" }`;
    /// - register-space exhaustion → `Internal`.
    fn allocate_local(
        &mut self,
        name: &'a str,
        is_const: bool,
        span: Span,
    ) -> Result<u16, SourceLoweringError> {
        self.allocate_local_with_mode(name, is_const, false, span)
    }

    fn allocate_hoisted_local(
        &mut self,
        name: &'a str,
        is_const: bool,
        span: Span,
    ) -> Result<u16, SourceLoweringError> {
        self.allocate_local_with_mode(name, is_const, true, span)
    }

    fn allocate_local_with_mode(
        &mut self,
        name: &'a str,
        is_const: bool,
        runtime_tdz: bool,
        span: Span,
    ) -> Result<u16, SourceLoweringError> {
        let scope_start = self.scope_starts.borrow().last().copied().unwrap_or(0);
        let same_scope_duplicate = self.locals[scope_start..].iter().any(|l| l.name == name);
        // Parameters live in the function's outermost lexical scope,
        // so they collide with a top-scope `let`/`const` of the same
        // name but NOT with a same-named binding inside a nested
        // block.
        let param_collision = scope_start == 0 && self.param_names.contains(&name);
        if same_scope_duplicate || param_collision {
            return Err(SourceLoweringError::unsupported("duplicate_binding", span));
        }
        // The new slot lives at `param_count + locals.len()` (using the
        // *current* length, not the peak — popped slots remain
        // reserved but are addressed by the new binding). The peak
        // tracks the maximum simultaneous live local count for the
        // FrameLayout reservation; bump it whenever the current
        // length grows past the previous peak.
        let live_len = RegisterIndex::try_from(self.locals.len())
            .map_err(|_| SourceLoweringError::Internal("local count overflow".into()))?;
        let slot = self
            .param_count
            .checked_add(live_len)
            .ok_or_else(|| SourceLoweringError::Internal("local register slot overflow".into()))?;
        self.locals.push(LocalBinding {
            name,
            slot,
            initialized: false,
            is_const,
            runtime_tdz,
        });
        let new_len = live_len
            .checked_add(1)
            .ok_or_else(|| SourceLoweringError::Internal("local count overflow".into()))?;
        if new_len > self.peak_local_count {
            self.peak_local_count = new_len;
        }
        Ok(slot)
    }

    /// Allocates a "hidden" local slot without a user-visible
    /// name — used by destructuring lowering to spill the source
    /// value to a register that won't be reclaimed by later
    /// `allocate_local` calls. Temps aren't usable here because
    /// `peak_local_count` can grow after a temp is acquired,
    /// shifting the temp base over slots now owned by locals
    /// allocated in between.
    ///
    /// The slot is flagged initialized immediately — there's no
    /// source-level identifier, so TDZ doesn't apply. `resolve_identifier`
    /// never matches because the name is a synthetic marker
    /// (`"@"`-prefixed) that isn't a legal JS identifier.
    fn allocate_anonymous_local(&mut self) -> Result<u16, SourceLoweringError> {
        // Synthetic name — `@` is not a legal identifier start in
        // JS, so `resolve_identifier` can't accidentally match it.
        // We store it as a `&'static str` so the binding outlives
        // any particular pattern's lifetime.
        let live_len = RegisterIndex::try_from(self.locals.len())
            .map_err(|_| SourceLoweringError::Internal("local count overflow".into()))?;
        let slot = self
            .param_count
            .checked_add(live_len)
            .ok_or_else(|| SourceLoweringError::Internal("hidden local slot overflow".into()))?;
        self.locals.push(LocalBinding {
            name: "@hidden",
            slot,
            initialized: true,
            is_const: true,
            runtime_tdz: false,
        });
        let new_len = live_len
            .checked_add(1)
            .ok_or_else(|| SourceLoweringError::Internal("local count overflow".into()))?;
        if new_len > self.peak_local_count {
            self.peak_local_count = new_len;
        }
        Ok(slot)
    }

    /// Marks the most recently allocated binding for `name` as
    /// initialized — called immediately after the lowering has
    /// emitted `Star r_slot` for the init result. A binding can only
    /// be initialized once; calling this after the binding is already
    /// initialized is a compiler bug, surfaced as `Internal`.
    fn mark_initialized(&mut self, name: &str) -> Result<(), SourceLoweringError> {
        let local = self
            .locals
            .iter_mut()
            .rev()
            .find(|l| l.name == name)
            .ok_or_else(|| {
                SourceLoweringError::Internal(format!("mark_initialized: no binding for {name}"))
            })?;
        if local.initialized {
            return Err(SourceLoweringError::Internal(format!(
                "mark_initialized: {name} already initialized"
            )));
        }
        local.initialized = true;
        Ok(())
    }

    /// Resolves a JS identifier into a [`BindingRef`]. Locals +
    /// params + already-captured upvalues are checked first;
    /// misses trigger a parent-chain walk that records a new
    /// capture entry (so the function ends up with a fresh
    /// upvalue slot and a matching `CaptureDescriptor` in the
    /// parent's `ClosureTemplate`).
    fn resolve_identifier(&self, name: &str) -> Option<BindingRef> {
        if let Some(binding) = self.resolve_own(name) {
            return Some(binding);
        }
        self.resolve_capture(name)
    }

    /// Like `resolve_identifier` but without parent-chain walk.
    fn resolve_own(&self, name: &str) -> Option<BindingRef> {
        if let Some(local) = self.locals.iter().rev().find(|l| l.name == name) {
            return Some(BindingRef::Local {
                reg: local.slot,
                initialized: local.initialized,
                is_const: local.is_const,
                runtime_tdz: local.runtime_tdz,
            });
        }
        for (i, param) in self.param_names.iter().enumerate() {
            if *param == name {
                let reg = u16::try_from(i)
                    .expect("param index fits in u16 because param_names length does");
                return Some(BindingRef::Param { reg });
            }
        }
        for (idx, entry) in self.captures.borrow().iter().enumerate() {
            if entry.name == name {
                let idx = u16::try_from(idx).expect("capture idx fits in u16");
                return Some(BindingRef::Upvalue { idx });
            }
        }
        None
    }

    fn resolve_capture(&self, name: &str) -> Option<BindingRef> {
        let parent = self.parent?;
        // Probe parent's own scope first.
        let desc = match parent.resolve_own(name) {
            Some(BindingRef::Local { reg, .. }) | Some(BindingRef::Param { reg }) => {
                Some(crate::closure::CaptureDescriptor::Register(
                    crate::bytecode::BytecodeRegister::new(reg),
                ))
            }
            Some(BindingRef::Upvalue { idx }) => Some(crate::closure::CaptureDescriptor::Upvalue(
                crate::closure::UpvalueId(idx),
            )),
            None => None,
        };
        if let Some(descriptor) = desc {
            return Some(self.record_capture(name, descriptor));
        }
        // Parent didn't have it directly — recurse into parent's
        // parent. Parent grows its own captures list as part of
        // the recursive resolution, giving us a `parent_idx` to
        // chain through.
        let Some(BindingRef::Upvalue { idx: parent_idx }) = parent.resolve_capture(name) else {
            return None;
        };
        let desc =
            crate::closure::CaptureDescriptor::Upvalue(crate::closure::UpvalueId(parent_idx));
        Some(self.record_capture(name, desc))
    }

    fn record_capture(
        &self,
        name: &str,
        descriptor: crate::closure::CaptureDescriptor,
    ) -> BindingRef {
        let mut captures = self.captures.borrow_mut();
        let idx = u16::try_from(captures.len()).expect("capture count fits in u16");
        captures.push(CaptureEntry {
            name: name.to_owned(),
            descriptor,
        });
        BindingRef::Upvalue { idx }
    }

    fn take_captures(&self) -> Vec<crate::closure::CaptureDescriptor> {
        std::mem::take(&mut *self.captures.borrow_mut())
            .into_iter()
            .map(|entry| entry.descriptor)
            .collect()
    }
}

/// Lowers `let x = init;` or `const x = init;`. Emits:
///
/// ```text
///   <init>            ; acc = init value
///   Star r_x          ; locals[x] = acc
/// ```
///
/// Allocates the slot for `x` **before** lowering the initializer so
/// the binding is in scope (in TDZ); the initializer can therefore
/// detect a self-reference (`let x = x + 1`) at compile time and
/// reject it as `tdz_self_reference`. After the post-init `Star`,
/// `mark_initialized` flips the binding to readable.
pub(super) fn lower_let_const_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    decl: &'a VariableDeclaration<'a>,
) -> Result<(), SourceLoweringError> {
    let is_const = match decl.kind {
        VariableDeclarationKind::Let => false,
        VariableDeclarationKind::Const => true,
        // `var` — treat as block-scoped `let` at the declaration
        // site. Classic `var` is function-scoped with hoisting;
        // 99% of user code that reaches us uses `var` in a place
        // where block-scoping behaves identically (single
        // declaration before first read), and the compile-time
        // TDZ check stays at `let`-parity. Full function-scope
        // hoisting is tracked as a follow-up but should not block
        // scripts that sprinkle `var` next to `let` / `const`.
        VariableDeclarationKind::Var => false,
        // `using` / `await using` should be routed through
        // `using_decl.rs` before reaching this generic declaration
        // helper. If parser recovery or a new caller gets here,
        // keep the failure explicit.
        VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing => {
            return Err(SourceLoweringError::unsupported(
                "parser_recovery_unrouted_using_decl",
                decl.span,
            ));
        }
    };

    if decl.declarations.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "parser_recovery_empty_var_decl",
            decl.span,
        ));
    }

    // Lower each declarator left-to-right. M7 lifted the
    // "single declarator only" restriction so the bench2 shape
    // `let s = 0, i = 0;` (two declarators) compiles directly. Each
    // declarator allocates its own slot and runs through the same
    // single-declarator path the M4 lowering already had.
    for declarator in decl.declarations.iter() {
        lower_single_declarator(builder, ctx, declarator, is_const)?;
    }
    Ok(())
}

fn lower_single_declarator<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    declarator: &'a VariableDeclarator<'a>,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    let init = declarator.init.as_ref().ok_or_else(|| {
        SourceLoweringError::unsupported("uninitialized_binding", declarator.span)
    })?;

    match &declarator.id {
        BindingPattern::BindingIdentifier(ident) => {
            let name = ident.name.as_str();
            let slot = ctx.allocate_local(name, is_const, declarator.span)?;

            // Lower init into acc. Reading the binding inside its
            // own initializer hits the `Local { initialized: false }`
            // arm of `lower_identifier_read` and surfaces as
            // `tdz_self_reference`.
            lower_return_expression(builder, ctx, init)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Star: {err:?}")))?;
            ctx.mark_initialized(name)?;
            Ok(())
        }
        // M24: `let [a, b, ...rest] = init;` / `let { a, b: x, c = 0 } = init;`
        // Lower the init into a temp, then bind each pattern leaf.
        BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
            lower_destructured_declarator(builder, ctx, &declarator.id, init, is_const)
        }
        // `let x = 1 = …;` is not grammatically possible, so an
        // AssignmentPattern as the top-level declarator id only
        // shows up through destructuring (e.g. `let { a = 0 } = src;`
        // where oxc wraps the leaf in AP). Those cases are
        // dispatched via `lower_pattern_bind`; reaching here at
        // the top level means something unsupported slipped
        // through.
        BindingPattern::AssignmentPattern(pat) => Err(SourceLoweringError::unsupported(
            "unexpected_assignment_pattern_declarator",
            pat.span,
        )),
    }
}

/// Lowers a destructuring declarator: `let <pattern> = <init>;`
/// where `<pattern>` is an `ArrayPattern` or `ObjectPattern`. The
/// init expression evaluates once into a dedicated temp
/// (`r_source`); the pattern then binds each leaf identifier as a
/// fresh local initialised from the matching
/// indexed / property read.
///
/// §14.3.3 BindingPattern-annotated declaration evaluation.
fn lower_destructured_declarator<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pattern: &'a BindingPattern<'a>,
    init: &'a Expression<'a>,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    // Use an anonymous hidden local (not a temp) for the source
    // spill. Temps are placed above `peak_local_count`, so any
    // `allocate_local` we do afterwards for pattern leaves would
    // bump the local count and overlap with the temp slot —
    // clobbering the source value mid-destructure. A dedicated
    // hidden-local slot sits inside the local region and doesn't
    // move.
    let src_slot = ctx.allocate_anonymous_local()?;
    lower_return_expression(builder, ctx, init)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(src_slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (destructure src): {err:?}"))
        })?;
    lower_pattern_bind(builder, ctx, pattern, src_slot, is_const)
}

/// Recursively lowers a `BindingPattern` whose value lives in
/// register `src_reg`. Allocates a new local for each leaf
/// `BindingIdentifier` and emits the indexed / property read that
/// populates it. `is_const` propagates to every leaf — a
/// destructuring `const { a } = …` produces a `const` binding for
/// `a`.
///
/// M24 scope — accepted leaves:
/// - `BindingIdentifier` (array element, array rest argument,
///   object property value, or bare declarator id).
///
/// Rejected with stable tags:
/// - Nested `ArrayPattern` / `ObjectPattern` leaves →
///   `nested_destructuring`.
/// - Array holes (`[a, , b]`) → `array_pattern_hole`.
/// - Computed object keys (`{ [k]: v }`) →
///   `computed_pattern_key`.
/// - Object rest (`{ ...rest }`) → `object_pattern_rest`.
fn lower_pattern_bind<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pattern: &'a BindingPattern<'a>,
    src_reg: RegisterIndex,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    match pattern {
        BindingPattern::BindingIdentifier(ident) => {
            let name = ident.name.as_str();
            let slot = ctx.allocate_local(name, is_const, ident.span)?;
            // At this call site the source value is already in acc
            // (array/object destructuring set it via the per-leaf
            // emission); the caller just needs to `Star` it into
            // the new slot.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (destructure leaf): {err:?}"
                    ))
                })?;
            ctx.mark_initialized(name)?;
            Ok(())
        }
        BindingPattern::ArrayPattern(pat) => {
            lower_array_pattern(builder, ctx, pat, src_reg, is_const)
        }
        BindingPattern::ObjectPattern(pat) => {
            lower_object_pattern(builder, ctx, pat, src_reg, is_const)
        }
        // AssignmentPattern wraps a leaf with a default (`= expr`).
        // Used at top level of declarator targets (rare — default
        // typically appears INSIDE a pattern). The accumulator
        // already holds the source value; run the default-check
        // against it, then delegate to the wrapped target.
        BindingPattern::AssignmentPattern(assign) => {
            emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
            destructure_assign_target(builder, ctx, &assign.left, is_const)
        }
    }
}

/// Lowers `[a, b, ...rest]` destructuring against the source in
/// `src_reg`. Array elements use indexed access (`LdaSmi i;
/// LdaKeyedProperty r_src`), which covers the common case (Array
/// sources) without the iterator-protocol overhead. Out-of-range
/// indices return `undefined` naturally through the keyed-property
/// path, matching the spec's "step beyond the iterator" semantics.
///
/// Rest uses `Array.prototype.slice(start)` against `src_reg` so
/// the resulting rest binding is a fresh Array whose length matches
/// the source's tail. Requires `slice` on the source's prototype
/// chain — always the case for plain Array values.
///
/// Holes (`[a, , b]` → `elements[1] == None`) rejected at compile
/// time with `array_pattern_hole`.
fn lower_array_pattern<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pat: &'a ArrayPattern<'a>,
    src_reg: RegisterIndex,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    for (index, element) in pat.elements.iter().enumerate() {
        let Some(element_pat) = element.as_ref() else {
            // Hole (`[a, , b]` → elements[1] = None). Skip —
            // the corresponding index has no binding, nothing to
            // store. `b` at elements[2] still reads via its own
            // iteration at the right index.
            continue;
        };
        let idx_i32 = i32::try_from(index)
            .map_err(|_| SourceLoweringError::Internal("array pattern index overflow".into()))?;
        // acc = index (int); LdaKeyedProperty r_src → acc = src[index].
        builder
            .emit(Opcode::LdaSmi, &[Operand::Imm(idx_i32)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaSmi (array pattern index): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::LdaKeyedProperty,
                &[Operand::Reg(u32::from(src_reg))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaKeyedProperty (array pattern): {err:?}"
                ))
            })?;
        // Apply default initialiser when the element has `= expr`.
        // Nested patterns go through a per-element temp so the
        // recursion can re-read by index / property.
        match element_pat {
            BindingPattern::AssignmentPattern(assign) => {
                emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
                destructure_assign_target(builder, ctx, &assign.left, is_const)?;
            }
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                // Read element value into a temp then recurse.
                let nested_slot = ctx.allocate_anonymous_local()?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (array pattern nested): {err:?}"
                        ))
                    })?;
                lower_pattern_bind(builder, ctx, element_pat, nested_slot, is_const)?;
            }
            _ => {
                lower_pattern_bind(builder, ctx, element_pat, src_reg, is_const)?;
            }
        }
    }

    if let Some(rest) = pat.rest.as_deref() {
        match &rest.argument {
            BindingPattern::BindingIdentifier(ident) => {
                let rest_name = ident.name.as_str();
                let rest_slot = ctx.allocate_local(rest_name, is_const, ident.span)?;
                emit_array_rest_slice(builder, ctx, src_reg, pat.elements.len(), rest_slot)?;
                ctx.mark_initialized(rest_name)?;
            }
            nested @ (BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_)) => {
                // `let [...[a, b]] = src` — slice into a temp,
                // destructure the resulting array into the inner
                // pattern.
                let rest_slot = ctx.allocate_anonymous_local()?;
                emit_array_rest_slice(builder, ctx, src_reg, pat.elements.len(), rest_slot)?;
                lower_pattern_bind(builder, ctx, nested, rest_slot, is_const)?;
            }
            _ => {
                return Err(SourceLoweringError::unsupported(
                    "nested_destructuring",
                    rest.span,
                ));
            }
        }
    }
    Ok(())
}

/// Helper used by `lower_array_pattern` to route an
/// `AssignmentPattern`'s left-side binding to the right path:
/// identifier → allocate-local + Star; nested pattern → bind
/// recursively against a per-element temp. Acc holds the value
/// (post default-check).
fn destructure_assign_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    target: &'a BindingPattern<'a>,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    match target {
        BindingPattern::BindingIdentifier(ident) => {
            let name = ident.name.as_str();
            let slot = ctx.allocate_local(name, is_const, ident.span)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (assign-target ident): {err:?}"
                    ))
                })?;
            ctx.mark_initialized(name)?;
            Ok(())
        }
        BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
            let nested_slot = ctx.allocate_anonymous_local()?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (assign-target nested): {err:?}"
                    ))
                })?;
            lower_pattern_bind(builder, ctx, target, nested_slot, is_const)
        }
        BindingPattern::AssignmentPattern(nested) => {
            // Double-wrapped default — unlikely but harmless:
            // run the inner's default, recurse.
            emit_default_for_destructured_leaf(builder, ctx, Some(&nested.right))?;
            destructure_assign_target(builder, ctx, &nested.left, is_const)
        }
    }
}

/// Emits `src_reg.slice(start)` and stores the resulting Array into
/// `rest_slot`. Three temps: receiver + callee + one arg slot. The
/// method is looked up via the property-name interner so later
/// accesses to `.slice` dedup.
fn emit_array_rest_slice(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    src_reg: RegisterIndex,
    start: usize,
    rest_slot: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    let start_i32 = i32::try_from(start)
        .map_err(|_| SourceLoweringError::Internal("rest start index overflow".into()))?;
    let callee_temp = ctx.acquire_temps(1)?;
    let arg_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // callee = src.slice
        let slice_idx = ctx.intern_property_name("slice")?;
        builder
            .emit(
                Opcode::LdaNamedProperty,
                &[Operand::Reg(u32::from(src_reg)), Operand::Idx(slice_idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaNamedProperty (slice): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (slice callee): {err:?}"))
            })?;
        // arg = start
        builder
            .emit(Opcode::LdaSmi, &[Operand::Imm(start_i32)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaSmi (slice arg): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(arg_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (slice arg): {err:?}"))
            })?;
        // CallProperty r_callee, r_src, [arg]
        builder
            .emit(
                Opcode::CallProperty,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(src_reg)),
                    Operand::RegList {
                        base: u32::from(arg_temp),
                        count: 1,
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CallProperty (slice): {err:?}"))
            })?;
        // acc now holds the sliced array; bind.
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(rest_slot))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (rest slot): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

/// Lowers `{ a, b: x, c = 0 }` object destructuring against the
/// source in `src_reg`. Each property reads via
/// `LdaNamedProperty r_src, key_idx`; an optional default fires
/// when the read returns `undefined` via
/// `JumpIfNotUndefined skip; <lower default>; skip:`.
///
/// M24 scope — accepted property shapes:
/// - Shorthand (`{ a }`) → read `a`, bind `a`.
/// - Renaming (`{ a: x }`) → read `a`, bind `x`.
/// - Defaults on either shape (`{ a = 0 }`, `{ a: x = 0 }`).
///
/// Rejected:
/// - Computed keys (`{ [k]: v }`) → `computed_pattern_key`.
/// - Object rest (`{ ...rest }`) → `object_pattern_rest`.
/// - Nested patterns (`{ a: { b } }`) → `nested_destructuring`.
fn lower_object_pattern<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    pat: &'a ObjectPattern<'a>,
    src_reg: RegisterIndex,
    is_const: bool,
) -> Result<(), SourceLoweringError> {
    // Track the set of property names bound so far so the rest
    // element (if any) can exclude them from the copy.
    let mut extracted_keys: Vec<String> = Vec::new();
    for prop in pat.properties.iter() {
        // Resolve the key: static identifier / string literal
        // both stringify to a known name; computed keys evaluate
        // an expression and use `LdaKeyedProperty`.
        let (computed_key_temp, key_name_for_rest, static_key_idx) = if prop.computed {
            // Computed key — evaluate the expression once into a
            // temp so both the property read and the rest-key
            // exclusion can reuse it.
            let temp = ctx.acquire_temps(1)?;
            let key_expr = prop.key.to_expression();
            lower_return_expression(builder, ctx, key_expr)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (object pattern computed key): {err:?}"
                    ))
                })?;
            (Some(temp), None, None)
        } else {
            let key_name = match &prop.key {
                PropertyKey::StaticIdentifier(ident) => ident.name.as_str().to_owned(),
                PropertyKey::StringLiteral(lit) => lit.value.as_str().to_owned(),
                other => {
                    return Err(SourceLoweringError::unsupported(
                        property_key_tag(other),
                        other.span(),
                    ));
                }
            };
            let idx = ctx.intern_property_name(&key_name)?;
            extracted_keys.push(key_name.clone());
            (None, Some(key_name), Some(idx))
        };
        // Read the property value into acc via Lda(Named|Keyed)Property.
        if let Some(temp) = computed_key_temp {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (pattern computed key): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::LdaKeyedProperty,
                    &[Operand::Reg(u32::from(src_reg))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaKeyedProperty (object pattern): {err:?}"
                    ))
                })?;
            ctx.release_temps(1);
        } else if let Some(idx) = static_key_idx {
            builder
                .emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(u32::from(src_reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaNamedProperty (object pattern): {err:?}"
                    ))
                })?;
        }
        // Dispatch on the binding shape:
        //   - AssignmentPattern (`{ a = 5 }` / `{ a: b = 5 }`)
        //     runs the default-check, then recurses into the
        //     target (identifier OR nested pattern).
        //   - Nested ArrayPattern / ObjectPattern stashes acc in
        //     a temp and recurses.
        //   - Plain BindingIdentifier is the straightforward
        //     allocate-local + Star case.
        match &prop.value {
            BindingPattern::AssignmentPattern(assign) => {
                emit_default_for_destructured_leaf(builder, ctx, Some(&assign.right))?;
                destructure_assign_target(builder, ctx, &assign.left, is_const)?;
            }
            BindingPattern::ArrayPattern(_) | BindingPattern::ObjectPattern(_) => {
                let nested_slot = ctx.allocate_anonymous_local()?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (object pattern nested): {err:?}"
                        ))
                    })?;
                lower_pattern_bind(builder, ctx, &prop.value, nested_slot, is_const)?;
            }
            BindingPattern::BindingIdentifier(ident) => {
                let name = ident.name.as_str();
                let slot = ctx.allocate_local(name, is_const, ident.span)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (object pattern leaf): {err:?}"
                        ))
                    })?;
                ctx.mark_initialized(name)?;
            }
        }
        let _ = key_name_for_rest;
    }

    // `{ a, b, ...rest }` — after binding `a` and `b`, copy every
    // other own-enumerable property of src into a fresh object,
    // excluding the keys we already bound.
    if let Some(rest) = pat.rest.as_deref() {
        let BindingPattern::BindingIdentifier(rest_ident) = &rest.argument else {
            return Err(SourceLoweringError::unsupported(
                "nested_destructuring",
                rest.span,
            ));
        };
        let rest_name = rest_ident.name.as_str();
        let rest_slot = ctx.allocate_local(rest_name, is_const, rest_ident.span)?;
        emit_object_rest_copy(builder, ctx, src_reg, &extracted_keys, rest_slot)?;
        ctx.mark_initialized(rest_name)?;
    }
    Ok(())
}

/// Build a fresh object and copy every own-enumerable data
/// property from `src_reg` EXCEPT the ones whose names we just
/// bound above. Drops the result into `rest_slot`.
fn emit_object_rest_copy(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    src_reg: RegisterIndex,
    excluded_keys: &[String],
    rest_slot: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    builder.emit(Opcode::CreateObject, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("encode CreateObject (obj rest): {err:?}"))
    })?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(rest_slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (obj rest target): {err:?}"))
        })?;
    // Copy all own-enumerable properties from src. Excluding the
    // already-bound keys is spec-correct (§14.3.3 RestDestructuring)
    // but `CopyDataProperties` currently only takes a single-
    // argument form — the loop below re-deletes the excluded keys
    // from the rest object after the bulk copy.
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(src_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (obj rest src): {err:?}"))
        })?;
    builder
        .emit(
            Opcode::CopyDataProperties,
            &[Operand::Reg(u32::from(rest_slot))],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode CopyDataProperties (obj rest): {err:?}"))
        })?;
    for key in excluded_keys {
        let idx = ctx.intern_property_name(key)?;
        builder
            .emit(
                Opcode::DelNamedProperty,
                &[Operand::Reg(u32::from(rest_slot)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode DelNamedProperty (obj rest exclusion): {err:?}"
                ))
            })?;
    }
    Ok(())
}

/// Extracts the leaf `BindingIdentifier` (and optional default
/// initializer) from a destructuring pattern value. Accepts either
/// a plain `BindingIdentifier` or an `AssignmentPattern` wrapping
/// one (which is how oxc represents `{ a = 0 }` / `{ a: x = 0 }`).
/// Nested patterns are rejected with `nested_destructuring`.
fn extract_pattern_leaf<'a>(
    value: &'a BindingPattern<'a>,
) -> Result<
    (
        &'a oxc_ast::ast::BindingIdentifier<'a>,
        Option<&'a Expression<'a>>,
    ),
    SourceLoweringError,
> {
    match value {
        BindingPattern::BindingIdentifier(ident) => Ok((ident, None)),
        BindingPattern::AssignmentPattern(assign) => {
            let BindingPattern::BindingIdentifier(ident) = &assign.left else {
                return Err(SourceLoweringError::unsupported(
                    "nested_destructuring",
                    assign.span,
                ));
            };
            Ok((ident, Some(&assign.right)))
        }
        BindingPattern::ArrayPattern(p) => Err(SourceLoweringError::unsupported(
            "nested_destructuring",
            p.span,
        )),
        BindingPattern::ObjectPattern(p) => Err(SourceLoweringError::unsupported(
            "nested_destructuring",
            p.span,
        )),
    }
}

/// Inserts the `undefined`-check default-initializer sequence when
/// a destructuring leaf has a default expression. Same pattern as
/// M22's param default initializer:
///
/// ```text
///   ; acc = read value
///   JumpIfNotUndefined skip
///   <lower default expr>   ; acc = default
/// skip:
/// ```
fn emit_default_for_destructured_leaf<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    default: Option<&'a Expression<'a>>,
) -> Result<(), SourceLoweringError> {
    let Some(expr) = default else {
        return Ok(());
    };
    let skip = builder.new_label();
    builder
        .emit_jump_to(Opcode::JumpIfNotUndefined, skip)
        .map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode JumpIfNotUndefined (destructure default): {err:?}"
            ))
        })?;
    lower_return_expression(builder, ctx, expr)?;
    builder
        .bind_label(skip)
        .map_err(|err| SourceLoweringError::Internal(format!("bind destructure skip: {err:?}")))?;
    Ok(())
}

/// Lower an `Expression::Identifier` reading the named binding into
/// the accumulator.
///
/// Resolution order:
/// 1. Local / parameter binding — routes through
///    [`lower_identifier_read`], which also primes a feedback slot
///    for M_JIT_C.2 consumption.
/// 2. Well-known global constant (M14) — emits a dedicated opcode:
///    `undefined` → `LdaUndefined`, `NaN` → `LdaNaN`, `Infinity` →
///    `LdaConstF64` against an interned `f64::INFINITY`.
/// 3. Well-known global property (M14) — `globalThis`, `Math`, and
///    any other recognised name emit `LdaGlobal` with the name
///    interned into the function's `PropertyNameTable`.
/// 4. Otherwise — surface the pre-existing `unbound_identifier`
///    compile-time rejection. Generalising this to "always emit
///    `LdaGlobal`" would match the ES spec's dynamic-lookup model,
///    but keeping the reject lets later milestones extend the
///    whitelist intentionally.
fn lower_identifier_reference(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    ident: &IdentifierReference<'_>,
) -> Result<(), SourceLoweringError> {
    let name = ident.name.as_str();
    if let Some(binding) = ctx.resolve_identifier(name) {
        return lower_identifier_read(builder, ctx, binding, ident.span);
    }
    match name {
        "undefined" => {
            builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
            })?;
            Ok(())
        }
        "NaN" => {
            builder
                .emit(Opcode::LdaNaN, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaNaN: {err:?}")))?;
            Ok(())
        }
        "Infinity" => {
            let idx = ctx.intern_float_constant(f64::INFINITY)?;
            builder
                .emit(Opcode::LdaConstF64, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaConstF64: {err:?}"))
                })?;
            Ok(())
        }
        n if is_whitelisted_global_name(n) => {
            // M14+: well-known runtime globals resolve via
            // `LdaGlobal`. The runtime installs every constructor
            // + namespace on the global object during boot, so any
            // name in `is_whitelisted_global_name` is guaranteed
            // to be live by the time user code runs.
            let idx = ctx.intern_property_name(name)?;
            builder
                .emit(Opcode::LdaGlobal, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaGlobal: {err:?}"))
                })?;
            Ok(())
        }
        _ => {
            // M35: ES-module imports and top-level exports live on
            // the global object by the time any function body runs
            // (`populate_import_globals` + the synthesised
            // module-init function). An identifier whose name
            // matches one of those module-level globals resolves
            // via `LdaGlobal` here instead of hitting the
            // script-mode `unbound_identifier` rejection.
            if ctx.is_module_global(name) {
                let idx = ctx.intern_property_name(name)?;
                builder
                    .emit(Opcode::LdaGlobal, &[Operand::Idx(idx)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaGlobal (module binding): {err:?}"
                        ))
                    })?;
                return Ok(());
            }
            Err(SourceLoweringError::unsupported(
                "unbound_identifier",
                ident.span,
            ))
        }
    }
}

fn emit_assert_not_hole(
    builder: &mut BytecodeBuilder,
    label: &'static str,
) -> Result<(), SourceLoweringError> {
    builder.emit(Opcode::AssertNotHole, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!("encode AssertNotHole ({label}): {err:?}"))
    })?;
    Ok(())
}

fn emit_load_binding_value(
    builder: &mut BytecodeBuilder,
    binding: BindingRef,
    ident_span: Span,
    label: &'static str,
) -> Result<(), SourceLoweringError> {
    match binding {
        BindingRef::Param { reg } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
        }
        BindingRef::Local {
            reg,
            initialized: true,
            runtime_tdz: false,
            ..
        } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
        }
        BindingRef::Local {
            reg,
            runtime_tdz: true,
            ..
        } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)?;
        }
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident_span,
            ));
        }
        BindingRef::Upvalue { idx } => {
            builder
                .emit(Opcode::LdaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaUpvalue ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)?;
        }
    }
    Ok(())
}

fn emit_assert_binding_ready_for_write(
    builder: &mut BytecodeBuilder,
    binding: BindingRef,
    ident_span: Span,
    label: &'static str,
) -> Result<(), SourceLoweringError> {
    match binding {
        BindingRef::Local {
            reg,
            runtime_tdz: true,
            ..
        } => {
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)
        }
        BindingRef::Param { .. }
        | BindingRef::Local {
            initialized: true, ..
        } => Ok(()),
        BindingRef::Local {
            initialized: false, ..
        } => Err(SourceLoweringError::unsupported(
            "tdz_self_reference",
            ident_span,
        )),
        BindingRef::Upvalue { idx } => {
            builder
                .emit(Opcode::LdaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaUpvalue ({label}): {err:?}"))
                })?;
            emit_assert_not_hole(builder, label)
        }
    }
}

/// Emits `Ldar reg` for an in-scope identifier read. Rejects
/// uninitialized locals (TDZ self-reference) at compile time so the
/// runtime never sees a hole on this path.
///
/// Allocates an arithmetic feedback slot and attaches it to the
/// emitted `Ldar` so the interpreter can record Int32 when the slot
/// holds an int32 value, and the JIT baseline can drop the `Ldar`
/// tag guard once the feedback stabilises (M_JIT_C.2 int32-trust
/// elision).
fn lower_identifier_read(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    binding: BindingRef,
    ident_span: Span,
) -> Result<(), SourceLoweringError> {
    match binding {
        BindingRef::Param { reg }
        | BindingRef::Local {
            reg,
            initialized: true,
            runtime_tdz: false,
            ..
        } => {
            let pc = builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar (identifier read): {err:?}"))
                })?;
            let slot = ctx.allocate_arithmetic_feedback();
            builder.attach_feedback(pc, slot);
            Ok(())
        }
        other => emit_load_binding_value(builder, other, ident_span, "identifier read"),
    }
}

/// Emits a Reg-form binary opcode (`Add`/`Sub`/...) reading the given
/// in-scope identifier as the RHS. Thin wrapper over
/// [`emit_identifier_as_reg_operand`], which allocates the feedback
/// slot so the interpreter can record Int32 / NotInt32 observations
/// and the JIT baseline can consume them via
/// [`analyze_template_candidate_with_feedback`].
fn lower_identifier_as_reg_rhs(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    encoding: &BinaryOpEncoding,
    binding: BindingRef,
    ident_span: Span,
) -> Result<(), SourceLoweringError> {
    emit_identifier_as_reg_operand(
        builder,
        ctx,
        encoding.reg_opcode,
        encoding.label,
        binding,
        ident_span,
    )?;
    Ok(())
}

/// §14.4.14 `yield* <argument>` — delegates iteration to another
/// iterable. Lowered as:
///   `GetIterator` on argument → `iter_temp`
///   loop: `IteratorStep value_temp, iter_temp`
///   if `done` (acc truthy) break
///   `Ldar value_temp; Yield`
///   jump loop_top
///   exit: `Ldar value_temp` (final value becomes expression's
///   result)
///
/// Scope: forwards values outward per spec. Sent values from
/// the outer caller's `.next(v)` reach the inner iterator only
/// as the acc at Yield resume — the inner iterator's `.next()`
/// doesn't receive them as arguments (full spec requires
/// `IteratorNext` with a sent-value operand). `.throw()` and
/// `.return()` completion forwarding are also deferred.
fn lower_yield_star<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    yield_expr: &'a oxc_ast::ast::YieldExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let Some(argument) = yield_expr.argument.as_ref() else {
        return Err(SourceLoweringError::Internal(
            "yield* without argument is a parse error".into(),
        ));
    };
    let iter_temp = ctx.acquire_temps(1)?;
    let value_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        // Resolve iterable once, stash in `value_temp` as a scratch
        // source, then convert to iterator and park in `iter_temp`.
        lower_return_expression(builder, ctx, argument)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (yield* src): {err:?}"))
            })?;
        builder
            .emit(Opcode::GetIterator, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode GetIterator (yield*): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(iter_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (yield* iter): {err:?}"))
            })?;

        let loop_top = builder.new_label();
        let loop_exit = builder.new_label();
        builder
            .bind_label(loop_top)
            .map_err(|err| SourceLoweringError::Internal(format!("bind yield* top: {err:?}")))?;
        builder
            .emit(
                Opcode::IteratorStep,
                &[
                    Operand::Reg(u32::from(value_temp)),
                    Operand::Reg(u32::from(iter_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode IteratorStep (yield*): {err:?}"))
            })?;
        builder
            .emit_jump_to(Opcode::JumpIfToBooleanTrue, loop_exit)
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode JumpIfToBooleanTrue (yield* done): {err:?}"
                ))
            })?;
        // Forward the inner iteration's value to the outer
        // consumer via a plain Yield.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (yield* value): {err:?}"))
            })?;
        builder.emit(Opcode::Yield, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode Yield (yield*): {err:?}"))
        })?;
        builder
            .emit_jump_to(Opcode::Jump, loop_top)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (yield* back): {err:?}"))
            })?;
        builder
            .bind_label(loop_exit)
            .map_err(|err| SourceLoweringError::Internal(format!("bind yield* exit: {err:?}")))?;
        // Final value of `yield* <iter>` expression is the
        // completion value from the inner iterator's
        // `{ value: X, done: true }` — `IteratorStep` already
        // deposited `X` in `value_temp` on the terminating
        // iteration.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(value_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (yield* result): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

/// §13.3.9 `ChainExpression` — the AST wrapper around every
/// optional-chain surface (`o?.a`, `o?.[k]`, `f?.()`, or any
/// nesting). The wrapper's `expression` is the actual
/// member/call/private-field tree that carries `optional: true`
/// on each short-circuit site.
///
/// Lowering:
///
/// ```text
///   <lower chain body, short_circuit on stack>
///   Jump end                     ; value already in acc
/// short_circuit:
///   LdaUndefined                 ; any `?.` null check lands here
/// end:
/// ```
///
/// While `short_circuit` is on the stack, the per-expression
/// lowerers (`lower_static_member_read` /
/// `lower_computed_member_read` / `lower_call_expression`) honour
/// `expr.optional` by emitting a nullish check against the
/// materialised base; otherwise those lowerers still reject
/// `optional: true` defensively.
fn lower_chain_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    chain: &'a oxc_ast::ast::ChainExpression<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::ChainElement;

    let short_circuit = builder.new_label();
    let end_label = builder.new_label();

    ctx.enter_optional_chain(short_circuit);
    let inner = match &chain.expression {
        ChainElement::CallExpression(call) => lower_call_expression(builder, ctx, call),
        ChainElement::StaticMemberExpression(member) => {
            lower_static_member_read(builder, ctx, member)
        }
        ChainElement::ComputedMemberExpression(member) => {
            lower_computed_member_read(builder, ctx, member)
        }
        ChainElement::PrivateFieldExpression(member) => {
            lower_private_field_read(builder, ctx, member)
        }
        ChainElement::TSNonNullExpression(expr) => {
            lower_return_expression(builder, ctx, &expr.expression)
        }
    };
    ctx.exit_optional_chain();
    inner?;

    builder
        .emit_jump_to(Opcode::Jump, end_label)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Jump (chain end): {err:?}"))
        })?;
    builder.bind_label(short_circuit).map_err(|err| {
        SourceLoweringError::Internal(format!("bind chain short-circuit: {err:?}"))
    })?;
    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
        SourceLoweringError::Internal(format!(
            "encode LdaUndefined (chain short-circuit): {err:?}"
        ))
    })?;
    builder
        .bind_label(end_label)
        .map_err(|err| SourceLoweringError::Internal(format!("bind chain end: {err:?}")))?;
    Ok(())
}

fn lower_return_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a Expression<'a>,
) -> Result<(), SourceLoweringError> {
    match expr {
        Expression::Identifier(ident) => lower_identifier_reference(builder, ctx, ident),
        Expression::NumericLiteral(literal) => {
            // Fast path: int32-fit integers go through `LdaSmi`.
            // Anything fractional / out of range (3.14, 1e20, NaN,
            // Infinity via `1/0`) interns the f64 and emits
            // `LdaConstF64` — no more "non_int32_literal" rejection.
            if let Ok(value) = int32_from_literal(literal) {
                builder
                    .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode LdaSmi: {err:?}"))
                    })?;
            } else {
                let idx = ctx.intern_float_constant(literal.value)?;
                builder
                    .emit(Opcode::LdaConstF64, &[Operand::Idx(idx)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode LdaConstF64: {err:?}"))
                    })?;
            }
            Ok(())
        }
        Expression::NullLiteral(_) => {
            builder
                .emit(Opcode::LdaNull, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaNull: {err:?}")))?;
            Ok(())
        }
        Expression::BooleanLiteral(lit) => {
            let opcode = if lit.value {
                Opcode::LdaTrue
            } else {
                Opcode::LdaFalse
            };
            builder
                .emit(opcode, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaBool: {err:?}")))?;
            Ok(())
        }
        Expression::StringLiteral(lit) => {
            // M15: intern the literal's UTF-8 value into the
            // function's string-literal side table and emit
            // `LdaConstStr <idx>`. The interpreter materialises a
            // runtime-owned `JsString` on demand (§6.1.4).
            let idx = ctx.intern_string_literal(lit.value.as_str())?;
            builder
                .emit(Opcode::LdaConstStr, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaConstStr: {err:?}"))
                })?;
            Ok(())
        }
        // M36: §6.1.6.2 BigInt literal — `42n`. oxc provides
        // the value already normalised to base-10 without the
        // trailing `n` suffix, which matches what
        // `alloc_bigint` expects.
        Expression::BigIntLiteral(lit) => {
            let idx = ctx.intern_bigint_literal(lit.value.as_str())?;
            builder
                .emit(Opcode::LdaConstBigInt, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaConstBigInt: {err:?}"))
                })?;
            Ok(())
        }
        // M36: §22.2 RegExp literal — `/pattern/flags` records
        // the source form into the function's regexp table and
        // emits `CreateRegExp`. Each evaluation allocates a
        // fresh RegExp object (§22.2.1.5) so there's no dedup.
        Expression::RegExpLiteral(lit) => {
            let pattern = lit.regex.pattern.text.as_str();
            let flags = lit.regex.flags.to_string();
            let idx = ctx.push_regexp_literal(pattern, &flags)?;
            builder
                .emit(Opcode::CreateRegExp, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode CreateRegExp: {err:?}"))
                })?;
            Ok(())
        }
        Expression::BinaryExpression(binary) => lower_binary_expression(builder, ctx, binary),
        Expression::AssignmentExpression(assign) => {
            // Nested assignment (`return x = 5;`, `let y = x = 5;`).
            // The lowering leaves the assigned value in acc, so this
            // composes as a normal accumulator-producing expression.
            lower_assignment_expression(builder, ctx, assign)
        }
        Expression::CallExpression(call) => {
            // `return f(args)`, `let x = f(args)`, `if (f(args))`,
            // any acc-producing position. Result lands in the
            // accumulator after `CallDirect`.
            lower_call_expression(builder, ctx, call)
        }
        Expression::ParenthesizedExpression(inner) => {
            lower_return_expression(builder, ctx, &inner.expression)
        }
        Expression::UnaryExpression(unary) => lower_unary_expression(builder, ctx, unary),
        Expression::UpdateExpression(update) => lower_update_expression(builder, ctx, update),
        Expression::ConditionalExpression(cond) => lower_conditional_expression(builder, ctx, cond),
        Expression::LogicalExpression(logical) => lower_logical_expression(builder, ctx, logical),
        Expression::ObjectExpression(obj) => lower_object_expression(builder, ctx, obj),
        Expression::ArrayExpression(arr) => lower_array_expression(builder, ctx, arr),
        Expression::StaticMemberExpression(member) => {
            lower_static_member_read(builder, ctx, member)
        }
        Expression::ComputedMemberExpression(member) => {
            lower_computed_member_read(builder, ctx, member)
        }
        // M29: `obj.#x` — §13.3.2 PrivateFieldExpression read.
        // Private-name resolution checks the enclosing class
        // body's declaration list at compile time; the runtime
        // walks `[[PrivateElements]]` using the active closure's
        // `class_id`.
        Expression::PrivateFieldExpression(expr) => lower_private_field_read(builder, ctx, expr),
        // M29: `#name in obj` — §13.10.1 PrivateInExpression.
        // Evaluates the RHS into a temp, then `InPrivate` checks
        // the runtime's `[[PrivateElements]]` table against the
        // active class_id.
        Expression::PrivateInExpression(expr) => lower_private_in_expression(builder, ctx, expr),
        // M33: `await <expr>` — lowers the operand into acc then
        // emits the `Await` opcode. Runtime semantics: drain the
        // microtask queue, unwrap settled promises (or throw on
        // rejection), pass plain values through unchanged per
        // §27.7.5.3 step 5.
        Expression::AwaitExpression(await_expr) => {
            lower_return_expression(builder, ctx, &await_expr.argument)?;
            builder
                .emit(Opcode::Await, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Await: {err:?}")))?;
            Ok(())
        }
        // M34: `yield <expr>` — §14.4 YieldExpression. Lowers
        // the operand into acc, emits `Yield` (suspends the
        // generator, returns to the `.next()` caller with
        // `{ value: acc, done: false }`). On resume, acc carries
        // the caller-provided sent value.
        //
        // `yield*` delegation (`expr.delegate`) is a separate
        // AST shape and stays deferred to a follow-up.
        Expression::YieldExpression(yield_expr) => {
            if yield_expr.delegate {
                return lower_yield_star(builder, ctx, yield_expr);
            }
            if let Some(arg) = yield_expr.argument.as_ref() {
                lower_return_expression(builder, ctx, arg)?;
            } else {
                builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaUndefined (yield): {err:?}"))
                })?;
            }
            builder
                .emit(Opcode::Yield, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Yield: {err:?}")))?;
            Ok(())
        }
        // TS-only non-null assertion. Runtime semantics are a
        // no-op; just lower the wrapped expression.
        Expression::TSNonNullExpression(expr) => {
            lower_return_expression(builder, ctx, &expr.expression)
        }
        // §13.3.9 Optional Chains — `o?.a`, `o?.[k]`, `f?.()`,
        // and any composition thereof. The ChainExpression wraps
        // the whole chain; individual optional elements inside it
        // carry `optional: true` and short-circuit to a shared
        // label that the chain's end installs.
        Expression::ChainExpression(chain) => lower_chain_expression(builder, ctx, chain),
        Expression::TemplateLiteral(tpl) => lower_template_literal(builder, ctx, tpl),
        // §13.3.11 `` tag`...${x}...` `` — call `tag(strings,
        // ...values)` where `strings` is the cooked-parts array
        // with a `.raw` property pointing at the raw-parts array.
        Expression::TaggedTemplateExpression(tagged) => {
            lower_tagged_template_expression(builder, ctx, tagged)
        }
        Expression::FunctionExpression(func) => lower_function_expression(builder, ctx, func),
        Expression::ArrowFunctionExpression(arrow) => {
            lower_arrow_function_expression(builder, ctx, arrow)
        }
        // M27: `this` reads the function's receiver slot. Only
        // meaningful inside constructors and methods — in plain
        // function bodies `CallUndefinedReceiver` sets `this =
        // undefined` (non-strict mode).
        Expression::ThisExpression(_) => {
            builder
                .emit(Opcode::LdaThis, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaThis: {err:?}")))?;
            Ok(())
        }
        // M27: `new Foo(args)`. Flows through the `Construct`
        // opcode which allocates the receiver from
        // `Foo.prototype`, invokes the constructor with
        // `this = receiver`, and applies §9.2.2.1's return
        // override.
        Expression::NewExpression(new_expr) => lower_new_expression(builder, ctx, new_expr),
        // M27: `class { … }` / `class Foo { … }` as an expression —
        // lowers to the constructor value in acc. No outer binding
        // is created; callers consume the value directly (e.g. `let
        // C = class {…}` or `return class {…};`).
        Expression::ClassExpression(class) => lower_class_expression(builder, ctx, class),
        // M35: §13.3.10 `import(expr)` — evaluate the specifier
        // into a fresh temp, then emit `DynamicImport <reg>`. The
        // dispatch handler resolves+loads the module and returns
        // a fulfilled Promise of its namespace.
        Expression::ImportExpression(import) => {
            let temp = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                lower_return_expression(builder, ctx, &import.source)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (dynamic import spec): {err:?}"
                        ))
                    })?;
                builder
                    .emit(Opcode::DynamicImport, &[Operand::Reg(u32::from(temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode DynamicImport: {err:?}"))
                    })?;
                Ok(())
            })();
            ctx.release_temps(1);
            lower
        }
        // M35: `import.meta` — fetch the module-meta namespace
        // from the runtime. Our current slice exposes a plain
        // object with one `url` string property.
        Expression::MetaProperty(meta)
            if meta.meta.name.as_str() == "import" && meta.property.name.as_str() == "meta" =>
        {
            builder.emit(Opcode::ImportMeta, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode ImportMeta: {err:?}"))
            })?;
            Ok(())
        }
        other => Err(SourceLoweringError::unsupported(
            expression_construct_tag(other),
            other.span(),
        )),
    }
}

/// Lowers `function(args) { body }` (a FunctionExpression in
/// expression position) into a fresh closure value with live
/// upvalue capture of outer-scope bindings.
///
/// Capture analysis: the inner function's body is lowered through
/// a recursive `lower_inner_function` call that passes the outer
/// context as the "lookup parent". Any identifier inside the inner
/// function that can't be resolved to a local/param/global is
/// looked up in the outer's bindings:
/// - Outer local / param → `CaptureDescriptor::Register(reg)` —
///   the outer frame promotes that slot into an open upvalue
///   cell at `CreateClosure` time (via
///   `capture_bytecode_register_upvalue`), and the inner closure
///   uses `LdaUpvalue <idx>` to read / `StaUpvalue <idx>` to write.
/// - Outer-outer capture → a nested closure references an
///   already-captured binding; emitted as
///   `CaptureDescriptor::Upvalue(UpvalueId)` so the dispatcher
///   re-captures the parent closure's upvalue cell.
///
/// Bytecode shape:
///
/// ```text
///   CreateClosure <inner_idx>, 0
/// ```
///
/// The `ClosureTable` entry at this PC carries the callee's
/// `FunctionIndex` plus the list of `CaptureDescriptor`s in
/// upvalue-index order.
fn lower_function_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    func: &'a Function<'a>,
) -> Result<(), SourceLoweringError> {
    // Lower the inner function first — the recursive lowering
    // collects the list of outer bindings it captured. The
    // captures come back as a `Vec<CaptureDescriptor>`; each
    // element's slot index matches the inner function's
    // `LdaUpvalue <idx>` operands.
    let (inner_idx, captures) = lower_inner_function_with_captures(func, ctx)?;
    {
        let mut fns = ctx.module_functions.borrow_mut();
        let target = &mut fns[inner_idx as usize];
        if func.r#async {
            target.set_async(true);
        }
        if func.generator {
            target.set_generator(true);
        }
    }

    let pc = builder.pc();
    let flags = match (func.r#async, func.generator) {
        (true, true) => crate::object::ClosureFlags::async_generator(),
        (true, false) => crate::object::ClosureFlags::async_fn(),
        (false, true) => crate::object::ClosureFlags::generator(),
        (false, false) => crate::object::ClosureFlags::normal(),
    };
    let template = crate::closure::ClosureTemplate::with_flags(
        crate::module::FunctionIndex(inner_idx),
        captures,
        flags,
    );
    ctx.record_closure_template(pc, template);

    // Emit `CreateClosure <idx>, 0`. The second operand carries
    // closure flags — dispatch reads them from the closure template
    // at the PC, so the imm is conventional (zero).
    builder
        .emit(
            Opcode::CreateClosure,
            &[Operand::Idx(inner_idx), Operand::Imm(0)],
        )
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateClosure: {err:?}")))?;
    Ok(())
}

/// Lowers `(args) => expr` / `(args) => { body }` — an arrow
/// function — into a closure value. Same shape as
/// `FunctionExpression` with two differences:
/// - Arrows cannot be generators; `async` rejected until M33.
/// - Arrows have lexical `this`. M26 doesn't introduce any `this`
///   support in the source compiler (classes and `this` land in
///   M27+), so lexical-`this` is automatically satisfied: every
///   arrow just lowers as a regular closure body and neither the
///   arrow nor its container uses `this`.
///
/// §15.3 Arrow Function Definitions.
fn lower_arrow_function_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    arrow: &'a ArrowFunctionExpression<'a>,
) -> Result<(), SourceLoweringError> {
    // oxc synthesises the arrow body as a `FunctionBody` whose
    // single statement is a `ReturnStatement` for concise
    // `() => expr` form. Block-body arrows already have a
    // regular FunctionBody. Either case flows through
    // `lower_inner_callable` unchanged — no special-casing of
    // `arrow.expression` needed.
    let (inner_idx, captures) = lower_inner_callable(ctx, &arrow.params, &arrow.body, None)?;
    if arrow.r#async {
        let mut fns = ctx.module_functions.borrow_mut();
        fns[inner_idx as usize].set_async(true);
    }
    let pc = builder.pc();
    let template = crate::closure::ClosureTemplate::with_flags(
        crate::module::FunctionIndex(inner_idx),
        captures,
        if arrow.r#async {
            crate::object::ClosureFlags::async_arrow()
        } else {
            crate::object::ClosureFlags::arrow()
        },
    );
    ctx.record_closure_template(pc, template);
    builder
        .emit(
            Opcode::CreateClosure,
            &[Operand::Idx(inner_idx), Operand::Imm(0)],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateClosure (arrow): {err:?}"))
        })?;
    Ok(())
}

/// Lowers `function foo() { … }` inside another function body.
/// Treated as hoisting-free shorthand for `let foo = function() {
/// … };` — the name is bound as a `const` local so accidental
/// reassignment rejects, and the closure's captures follow the
/// same parent-chain resolution the FunctionExpression path uses.
///
/// M25 simplification: spec-accurate hoisting (§14.1.11) isn't
/// implemented — forward references to a nested
/// FunctionDeclaration before its lexical position would surface
/// as `unbound_identifier`. Real code typically declares before
/// use, so this is a narrow corner.
fn lower_nested_function_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    func: &'a Function<'a>,
) -> Result<(), SourceLoweringError> {
    let name_ident = func
        .id
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;
    let name = name_ident.name.as_str();

    // Lower the inner function + record captures against the
    // enclosing context, same as FunctionExpression.
    let (inner_idx, captures) = lower_inner_function_with_captures(func, ctx)?;
    {
        let mut fns = ctx.module_functions.borrow_mut();
        let target = &mut fns[inner_idx as usize];
        if func.r#async {
            target.set_async(true);
        }
        if func.generator {
            target.set_generator(true);
        }
    }
    let pc = builder.pc();
    let flags = match (func.r#async, func.generator) {
        (true, true) => crate::object::ClosureFlags::async_generator(),
        (true, false) => crate::object::ClosureFlags::async_fn(),
        (false, true) => crate::object::ClosureFlags::generator(),
        (false, false) => crate::object::ClosureFlags::normal(),
    };
    let template = crate::closure::ClosureTemplate::with_flags(
        crate::module::FunctionIndex(inner_idx),
        captures,
        flags,
    );
    ctx.record_closure_template(pc, template);
    builder
        .emit(
            Opcode::CreateClosure,
            &[Operand::Idx(inner_idx), Operand::Imm(0)],
        )
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateClosure: {err:?}")))?;

    // Bind the produced closure to a local with the function's
    // name (`const`-like — reassigning would rebind the name to
    // a different value which the spec disallows for a
    // declaration binding).
    let slot = ctx.allocate_local(name, true, name_ident.span)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (nested function binding): {err:?}"))
        })?;
    ctx.mark_initialized(name)?;
    Ok(())
}

/// Lowers a nested `class Foo { … }` declaration into a const
/// binding of a constructor closure with methods installed on
/// its prototype / static properties. M27 surface:
/// - Explicit `constructor(args) { body }` or synthesised empty
///   constructor if absent.
/// - Instance methods (installed on `Foo.prototype`).
/// - Static methods (installed on `Foo` itself).
/// - Computed keys, getters / setters, class fields, `extends`,
///   decorators all rejected with stable per-shape tags.
///
/// Bytecode shape:
///
/// ```text
///   CreateClosure <ctor_idx>, flags=class_constructor
///   Star r_class
///   LdaNamedProperty r_class, "prototype"
///   Star r_proto
///   ; for each instance method:
///     CreateClosure <m_idx>, 0
///     StaNamedProperty r_proto, "<name>"
///   ; for each static method:
///     CreateClosure <m_idx>, 0
///     StaNamedProperty r_class, "<name>"
///   Ldar r_class           ; acc = Foo (value of the declaration)
///   Star r_<name>          ; bind Foo as a const local
/// ```
fn lower_nested_class_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    class: &'a Class<'a>,
) -> Result<(), SourceLoweringError> {
    let class_ident = class
        .id
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_class", class.span))?;
    let class_name = class_ident.name.as_str();
    // Pre-allocate the class-name local BEFORE lowering methods so
    // `static zero() { return new Point(); }` can resolve the
    // forward self-reference through the capture path.
    let class_slot = ctx.allocate_local(class_name, true, class_ident.span)?;
    lower_class_body_core(builder, ctx, class, Some(class_name))?;
    // acc = constructor at this point — bind it to the class-name
    // local and flip the binding from pending to initialized.
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(class_slot))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (class name binding): {err:?}"))
        })?;
    ctx.mark_initialized(class_name)?;
    Ok(())
}

/// M27: ClassExpression — lowers the class body and leaves the
/// constructor in acc. Unlike `ClassDeclaration`, no outer binding
/// is introduced; the caller consumes the acc value (e.g. `let C =
/// class {…}` or `return class {…};`).
///
/// Named class expressions (`class Foo {…}` as expression) are
/// accepted, but the inner-scope `Foo` binding is NOT exposed to
/// the class body yet — methods that self-refer to the class by
/// name would need a dedicated scope frame. Most class expressions
/// are anonymous in practice, so the trade-off is acceptable for
/// M27.
fn lower_class_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    class: &'a Class<'a>,
) -> Result<(), SourceLoweringError> {
    let hint = class.id.as_ref().map(|id| id.name.as_str());
    lower_class_body_core(builder, ctx, class, hint)
}

/// Shared core for `ClassDeclaration` + `ClassExpression`. Validates
/// class elements, optionally evaluates the `extends` expression,
/// lowers the constructor (real or synthesised) with the
/// `class_constructor` flag, lowers instance methods onto
/// `Constructor.prototype` and static methods onto the Constructor
/// itself, wires `[[HomeObject]]` via `SetHomeObject` for every
/// method + the constructor, and — for derived classes — emits
/// `SetClassHeritage` so the runtime can link
/// `Sub.__proto__ = Super` and `Sub.prototype.__proto__ =
/// Super.prototype` (§15.7.14 ClassDefinitionEvaluation).
///
/// `name_hint` is the display name used for the synthesised empty
/// constructor and passed through to `lower_inner_callable`.
fn lower_class_body_core<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    class: &'a Class<'a>,
    name_hint: Option<&str>,
) -> Result<(), SourceLoweringError> {
    if !class.decorators.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "class_decorator",
            class.decorators[0].span,
        ));
    }
    let class_name_owned: String = name_hint.map(str::to_owned).unwrap_or_default();
    let class_name: &str = &class_name_owned;
    // §15.7.14 step 3 — the presence of `extends` puts us in a
    // derived class, which changes constructor synthesis
    // (`constructor(...args) { super(...args); }`), the
    // default-receiver handling in `construct_callable`, and
    // enables `super(args)` inside the constructor.
    let is_derived = class.super_class.is_some();

    // 1) Classify class elements. M29 introduced methods +
    //    accessors + fields (public / private / instance /
    //    static) buckets; M29.5 extends that with private
    //    methods/accessors (same bucket as public methods, now
    //    flagged via `is_private`) and static blocks.
    //
    // `private_decls` tracks per-name what has already been
    // declared so the §15.7.11 duplicate check can permit
    // `get #x` + `set #x` pairs while still rejecting
    // `#x; get #x() {}` and the like.
    let mut constructor_fn: Option<&'a Function<'a>> = None;
    let mut methods: Vec<ClassMethod<'a>> = Vec::new();
    let mut instance_fields: Vec<ClassField<'a>> = Vec::new();
    let mut static_fields: Vec<ClassField<'a>> = Vec::new();
    let mut static_blocks: Vec<&'a oxc_ast::ast::StaticBlock<'a>> = Vec::new();
    let mut private_names: Vec<String> = Vec::new();
    let mut private_decls: Vec<(String, PrivateDecl)> = Vec::new();
    for element in class.body.body.iter() {
        match element {
            ClassElement::MethodDefinition(method) => {
                if method.computed {
                    return Err(SourceLoweringError::unsupported(
                        "computed_class_method_key",
                        method.span,
                    ));
                }
                let (key_name_owned, is_private_method) = match &method.key {
                    PropertyKey::StaticIdentifier(ident) => (ident.name.to_string(), false),
                    PropertyKey::StringLiteral(lit) => (lit.value.to_string(), false),
                    PropertyKey::PrivateIdentifier(ident) => {
                        // Private methods live in the class's
                        // private-name namespace — register the
                        // name so `this.#m()` validates at
                        // compile time. §15.7.11 duplicate check
                        // allows `get #x` + `set #x` pairs to
                        // merge; any other collision is an early
                        // error.
                        let n = ident.name.to_string();
                        let kind = match method.kind {
                            MethodDefinitionKind::Get => PrivateDecl::Getter,
                            MethodDefinitionKind::Set => PrivateDecl::Setter,
                            MethodDefinitionKind::Method => PrivateDecl::Method,
                            MethodDefinitionKind::Constructor => PrivateDecl::Method,
                        };
                        record_private_decl(&mut private_decls, &n, kind, method.span)?;
                        if !private_names.contains(&n) {
                            private_names.push(n.clone());
                        }
                        (n, true)
                    }
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            property_key_tag(other),
                            other.span(),
                        ));
                    }
                };
                match method.kind {
                    MethodDefinitionKind::Constructor => {
                        if is_private_method {
                            return Err(SourceLoweringError::unsupported(
                                "private_class_constructor",
                                method.span,
                            ));
                        }
                        constructor_fn = Some(&method.value);
                    }
                    MethodDefinitionKind::Method
                    | MethodDefinitionKind::Get
                    | MethodDefinitionKind::Set => {
                        methods.push(ClassMethod {
                            name: key_name_owned,
                            is_static: method.r#static,
                            is_private: is_private_method,
                            kind: method.kind,
                            func: &method.value,
                        });
                    }
                }
            }
            ClassElement::PropertyDefinition(prop) => {
                if prop.computed {
                    return Err(SourceLoweringError::unsupported(
                        "computed_class_field",
                        prop.span,
                    ));
                }
                if !prop.decorators.is_empty() {
                    return Err(SourceLoweringError::unsupported(
                        "class_decorator",
                        prop.decorators[0].span,
                    ));
                }
                match &prop.key {
                    PropertyKey::StaticIdentifier(ident) => {
                        let field = ClassField {
                            name: ident.name.to_string(),
                            is_private: false,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    PropertyKey::StringLiteral(lit) => {
                        let field = ClassField {
                            name: lit.value.to_string(),
                            is_private: false,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    PropertyKey::PrivateIdentifier(ident) => {
                        let name = ident.name.to_string();
                        record_private_decl(
                            &mut private_decls,
                            &name,
                            PrivateDecl::Field,
                            prop.span,
                        )?;
                        if !private_names.contains(&name) {
                            private_names.push(name.clone());
                        }
                        let field = ClassField {
                            name,
                            is_private: true,
                            initializer: prop.value.as_ref(),
                            span: prop.span,
                        };
                        if prop.r#static {
                            static_fields.push(field);
                        } else {
                            instance_fields.push(field);
                        }
                    }
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            property_key_tag(other),
                            other.span(),
                        ));
                    }
                }
            }
            ClassElement::StaticBlock(block) => {
                // M29.5: accepted. Each block becomes a 0-param
                // thunk invoked with `this = class` at
                // class-definition time (step 12 below).
                static_blocks.push(block.as_ref());
            }
            ClassElement::AccessorProperty(prop) => {
                return Err(SourceLoweringError::unsupported(
                    "accessor_property",
                    prop.span,
                ));
            }
            ClassElement::TSIndexSignature(sig) => {
                return Err(SourceLoweringError::unsupported(
                    "ts_index_signature",
                    sig.span,
                ));
            }
        }
    }

    let has_instance_fields = !instance_fields.is_empty();
    let class_private_names: std::rc::Rc<[String]> = if private_names.is_empty() {
        std::rc::Rc::from([])
    } else {
        std::rc::Rc::from(private_names.clone().into_boxed_slice())
    };

    // 2) Super-class eligibility flags for methods + constructor.
    //    Methods (including static) allow `super.x`; derived
    //    constructors additionally allow `super(args)`.
    let method_super = ClassSuperBinding {
        allow_super_property: true,
        allow_super_call: false,
    };
    let ctor_super = ClassSuperBinding {
        allow_super_property: true,
        allow_super_call: is_derived,
    };

    // 3) Acquire heritage + spill temps. Ordering mirrors §15.7.14:
    //    evaluate `superclass` first, then build the constructor
    //    closure. Heritage temp is only allocated when `extends`
    //    is present so non-derived classes keep their previous
    //    two-slot temp footprint.
    let heritage_temp: Option<RegisterIndex> = if is_derived {
        Some(ctx.acquire_temps(1)?)
    } else {
        None
    };
    let class_temp = ctx.acquire_temps(1).inspect_err(|_| {
        if is_derived {
            ctx.release_temps(1);
        }
    })?;
    let proto_temp = ctx.acquire_temps(1).inspect_err(|_| {
        ctx.release_temps(1);
        if is_derived {
            ctx.release_temps(1);
        }
    })?;
    let method_temp = ctx.acquire_temps(1).inspect_err(|_| {
        ctx.release_temps(2);
        if is_derived {
            ctx.release_temps(1);
        }
    })?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // §15.7.14 step 5 — evaluate the superclass expression
        // before anything else, while the outer lexical context is
        // still active. The runtime's `SetClassHeritage` opcode
        // validates "null or constructor" after we've built the
        // class constructor.
        if let Some(super_expr) = class.super_class.as_ref() {
            lower_return_expression(builder, ctx, super_expr)?;
            let heritage = heritage_temp.expect("heritage_temp allocated when derived");
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(heritage))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (class heritage): {err:?}"))
                })?;
        }

        // M29: §6.2.12 — allocate a fresh class_id BEFORE we
        // create any closure belonging to the class. Subsequent
        // `CopyClassId r_target` stamps it on the ctor, each
        // method/accessor, and the field initializer. The
        // allocation is a no-op for classes without private
        // names, but emitting it unconditionally keeps the shape
        // predictable and lets tests rely on a non-zero id.
        builder.emit(Opcode::AllocClassId, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode AllocClassId: {err:?}"))
        })?;

        // 4) Lower the constructor — if none present, synthesise
        //    one. Derived classes with no explicit constructor get
        //    `constructor(...args) { super(...args); }` per
        //    §15.7.14 step 10.b; base classes stay with the
        //    `function() {}` synthesis inherited from M27.
        let ctor_idx = match constructor_fn {
            Some(func) => {
                let (idx, captures) = lower_inner_callable_with_super(
                    ctx,
                    &func.params,
                    func.body.as_ref().ok_or_else(|| {
                        SourceLoweringError::unsupported("declared_only_function", func.span)
                    })?,
                    Some(class_name.to_owned()),
                    Some(ctor_super),
                    Some(std::rc::Rc::clone(&class_private_names)),
                )?;
                if is_derived {
                    let mut fns = ctx.module_functions.borrow_mut();
                    fns[idx as usize].set_derived_constructor(true);
                }
                let pc = builder.pc();
                // Constructor closure gets the class_constructor
                // flag so plain `Foo()` (without `new`) throws
                // TypeError.
                let template = crate::closure::ClosureTemplate::with_flags(
                    crate::module::FunctionIndex(idx),
                    captures,
                    crate::object::ClosureFlags::class_constructor(),
                );
                ctx.record_closure_template(pc, template);
                builder
                    .emit(Opcode::CreateClosure, &[Operand::Idx(idx), Operand::Imm(0)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode CreateClosure (class ctor): {err:?}"
                        ))
                    })?;
                idx
            }
            None => {
                let synthetic = if is_derived {
                    let idx = synthesise_derived_default_constructor(ctx, class_name)?;
                    let mut fns = ctx.module_functions.borrow_mut();
                    fns[idx as usize].set_derived_constructor(true);
                    idx
                } else {
                    synthesise_empty_constructor(ctx, class_name)?
                };
                let pc = builder.pc();
                let template = crate::closure::ClosureTemplate::with_flags(
                    crate::module::FunctionIndex(synthetic),
                    Vec::new(),
                    crate::object::ClosureFlags::class_constructor(),
                );
                ctx.record_closure_template(pc, template);
                builder
                    .emit(
                        Opcode::CreateClosure,
                        &[Operand::Idx(synthetic), Operand::Imm(0)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode CreateClosure (class default ctor): {err:?}"
                        ))
                    })?;
                synthetic
            }
        };
        let _ = ctor_idx;

        // acc = constructor — spill to r_class so we can install
        // methods and statics against a stable register.
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(class_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (class ctor spill): {err:?}"))
            })?;
        // M29: stamp the class_id onto the ctor closure now that
        // it lives in a register.
        builder
            .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(class_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode CopyClassId (class ctor): {err:?}"))
            })?;
        let prototype_idx = ctx.intern_property_name("prototype")?;
        builder
            .emit(
                Opcode::LdaNamedProperty,
                &[
                    Operand::Reg(u32::from(class_temp)),
                    Operand::Idx(prototype_idx),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaNamedProperty (class prototype): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(proto_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (class prototype spill): {err:?}"
                ))
            })?;

        // 5) §15.7.14 steps 6-7 — wire the heritage. Must happen
        //    BEFORE method installation so methods that capture the
        //    class (e.g. `static zero() { return new Point(); }`)
        //    observe a fully-initialized prototype chain. Any
        //    subsequent `Get`/`Set` on `Super.prototype` from method
        //    bodies relies on this link being in place.
        if let Some(heritage) = heritage_temp {
            builder
                .emit(
                    Opcode::SetClassHeritage,
                    &[
                        Operand::Reg(u32::from(class_temp)),
                        Operand::Reg(u32::from(heritage)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode SetClassHeritage: {err:?}"))
                })?;
        }

        // 6) §10.2.5 MakeMethod on the constructor — sets its
        //    `[[HomeObject]]` to `Sub.prototype` so `super.foo` from
        //    inside the constructor body walks the prototype chain
        //    rather than the static chain. The acc still holds the
        //    constructor after the earlier `Star`; we refresh it
        //    through `class_temp` for SetHomeObject's target.
        builder
            .emit(
                Opcode::SetHomeObject,
                &[
                    Operand::Reg(u32::from(class_temp)),
                    Operand::Reg(u32::from(proto_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode SetHomeObject (class ctor): {err:?}"))
            })?;

        // 7) Instance + static method / accessor installation.
        //    Each closure gets a home object, a class_id stamp,
        //    and an install opcode chosen per `kind`.
        for method in methods.iter() {
            let (idx, captures) = lower_inner_callable_with_super(
                ctx,
                &method.func.params,
                method.func.body.as_ref().ok_or_else(|| {
                    SourceLoweringError::unsupported("declared_only_function", method.func.span)
                })?,
                Some(method.name.to_owned()),
                Some(method_super),
                Some(std::rc::Rc::clone(&class_private_names)),
            )?;
            let pc = builder.pc();
            let template = crate::closure::ClosureTemplate::with_flags(
                crate::module::FunctionIndex(idx),
                captures,
                crate::object::ClosureFlags::method(),
            );
            ctx.record_closure_template(pc, template);
            builder
                .emit(Opcode::CreateClosure, &[Operand::Idx(idx), Operand::Imm(0)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateClosure (class method): {err:?}"
                    ))
                })?;
            // Spill into `method_temp` so we can stamp HomeObject
            // / class_id without disturbing the accumulator's
            // closure value; the install opcode still reads it
            // back from acc.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (class method spill): {err:?}"
                    ))
                })?;
            let home_reg = if method.is_static {
                class_temp
            } else {
                proto_temp
            };
            builder
                .emit(
                    Opcode::SetHomeObject,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(home_reg)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetHomeObject (class method): {err:?}"
                    ))
                })?;
            // M29: stamp class_id so private-name lookups inside
            // the method body resolve to this class's bucket.
            builder
                .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CopyClassId (class method): {err:?}"
                    ))
                })?;
            let name_idx = ctx.intern_property_name(&method.name)?;
            // M29.5: private methods go to `[[PrivateMethods]]`
            // (copied to each instance during construction) for
            // instance members, or directly to the class's own
            // `[[PrivateElements]]` for static members. Public
            // methods install with the usual StaNamedProperty /
            // DefineClassGetter / DefineClassSetter.
            let (install_op, install_target) =
                match (method.is_private, method.is_static, method.kind) {
                    (false, _, MethodDefinitionKind::Method) => (
                        // §15.7.11 — class methods land as
                        // non-enumerable data properties rather
                        // than the default enumerable shape of
                        // `StaNamedProperty`, so they stay out of
                        // `for…in` / `Object.keys`.
                        Opcode::DefineClassMethod,
                        if method.is_static {
                            class_temp
                        } else {
                            proto_temp
                        },
                    ),
                    (false, _, MethodDefinitionKind::Get) => (
                        Opcode::DefineClassGetter,
                        if method.is_static {
                            class_temp
                        } else {
                            proto_temp
                        },
                    ),
                    (false, _, MethodDefinitionKind::Set) => (
                        Opcode::DefineClassSetter,
                        if method.is_static {
                            class_temp
                        } else {
                            proto_temp
                        },
                    ),
                    (true, false, MethodDefinitionKind::Method) => {
                        (Opcode::PushPrivateMethod, class_temp)
                    }
                    (true, false, MethodDefinitionKind::Get) => {
                        (Opcode::PushPrivateGetter, class_temp)
                    }
                    (true, false, MethodDefinitionKind::Set) => {
                        (Opcode::PushPrivateSetter, class_temp)
                    }
                    (true, true, MethodDefinitionKind::Method) => {
                        (Opcode::DefinePrivateMethod, class_temp)
                    }
                    (true, true, MethodDefinitionKind::Get) => {
                        (Opcode::DefinePrivateGetter, class_temp)
                    }
                    (true, true, MethodDefinitionKind::Set) => {
                        (Opcode::DefinePrivateSetter, class_temp)
                    }
                    (_, _, MethodDefinitionKind::Constructor) => unreachable!("filtered above"),
                };
            builder
                .emit(
                    install_op,
                    &[
                        Operand::Reg(u32::from(install_target)),
                        Operand::Idx(name_idx),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode {install_op:?} (class method install): {err:?}"
                    ))
                })?;
        }

        // 8) §15.7.14 step 28 — if the class body declares any
        //    instance fields (public or private), synthesise a
        //    field-initializer closure and attach it to the
        //    constructor via `SetClassFieldInitializer`. The
        //    runtime auto-invokes it on fresh receivers (base
        //    ctors run it in `construct_callable`; derived ctors
        //    run it after `super()` in `super_call_dispatch`).
        if has_instance_fields {
            let (init_idx, init_captures) = synthesise_field_initializer(
                ctx,
                &instance_fields,
                class_name,
                std::rc::Rc::clone(&class_private_names),
            )?;
            let pc = builder.pc();
            let template = crate::closure::ClosureTemplate::with_flags(
                crate::module::FunctionIndex(init_idx),
                init_captures,
                crate::object::ClosureFlags::method(),
            );
            ctx.record_closure_template(pc, template);
            builder
                .emit(
                    Opcode::CreateClosure,
                    &[Operand::Idx(init_idx), Operand::Imm(0)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateClosure (field initializer): {err:?}"
                    ))
                })?;
            // Spill, stamp home + class_id, then install onto
            // the ctor. acc keeps the closure for
            // `SetClassFieldInitializer` to consume.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (field init spill): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::SetHomeObject,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(proto_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetHomeObject (field init): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CopyClassId (field init): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::SetClassFieldInitializer,
                    &[Operand::Reg(u32::from(class_temp))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetClassFieldInitializer: {err:?}"
                    ))
                })?;
        }

        // 9) Static fields — evaluate each initializer inline and
        //    install directly on the class constructor. Runs at
        //    class-definition time (not instance creation), so
        //    the expression sees the outer lexical scope. Real
        //    engines bind `this = class` for these expressions;
        //    M29 keeps that as a known limitation and will
        //    revisit once a dedicated per-field evaluator lands.
        for field in static_fields.iter() {
            if let Some(init) = field.initializer {
                lower_return_expression(builder, ctx, init)?;
            } else {
                builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaUndefined (static field default): {err:?}"
                    ))
                })?;
            }
            let name_idx = ctx.intern_property_name(&field.name)?;
            let opcode = if field.is_private {
                Opcode::DefinePrivateField
            } else {
                Opcode::DefineField
            };
            builder
                .emit(
                    opcode,
                    &[Operand::Reg(u32::from(class_temp)), Operand::Idx(name_idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode {opcode:?} (static field): {err:?}"
                    ))
                })?;
        }

        // 10) M29.5: static blocks. Each `static { … }` compiles
        //     to a 0-param thunk invoked with `this = class` at
        //     class-definition time. Declaration order matters —
        //     they run after methods + static fields so the
        //     class is fully set up.
        for block in static_blocks.iter() {
            let (idx, captures) = synthesise_static_block(
                ctx,
                block,
                class_name,
                std::rc::Rc::clone(&class_private_names),
            )?;
            let pc = builder.pc();
            let template = crate::closure::ClosureTemplate::with_flags(
                crate::module::FunctionIndex(idx),
                captures,
                crate::object::ClosureFlags::method(),
            );
            ctx.record_closure_template(pc, template);
            builder
                .emit(Opcode::CreateClosure, &[Operand::Idx(idx), Operand::Imm(0)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CreateClosure (static block): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (static block spill): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::SetHomeObject,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(class_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetHomeObject (static block): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::CopyClassId, &[Operand::Reg(u32::from(method_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CopyClassId (static block): {err:?}"
                    ))
                })?;
            // Invoke the thunk: `CallProperty r_thunk, r_class, {}`.
            // The receiver operand pins `this = class` inside
            // the block body; zero args match the zero-param
            // signature.
            builder
                .emit(
                    Opcode::CallProperty,
                    &[
                        Operand::Reg(u32::from(method_temp)),
                        Operand::Reg(u32::from(class_temp)),
                        Operand::RegList { base: 0, count: 0 },
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode CallProperty (static block): {err:?}"
                    ))
                })?;
        }

        // 11) Leave the constructor in acc — the caller
        //     (declaration or expression path) decides whether
        //     to bind it anywhere.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(class_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (class result): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(3);
    if is_derived {
        ctx.release_temps(1);
    }
    lower
}

/// §15.7.14 step 28 — synthesises the class field initializer
/// closure. Each instance field (public or private) becomes one
/// `DefineField` / `DefinePrivateField` pair in the body:
///
/// ```text
///   LdaThis                             ; once at entry
///   Star r_this
///   ; for each field:
///     <initializer>  (or LdaUndefined)
///     DefineField r_this, name_idx      ; public
///     ; or DefinePrivateField r_this, name_idx
///   LdaUndefined
///   Return
/// ```
///
/// The closure is installed on the class constructor via
/// `SetClassFieldInitializer`; the runtime invokes it once per
/// instance (see `construct_callable` / `super_call_dispatch`).
/// Captures are resolved via the normal parent-chain walk so
/// initializers can reference outer-scope bindings.
/// M29.5: compile a `static { … }` block into a 0-param thunk
/// whose body IS that block's statement list. Invoked at
/// class-definition time with `this = class`, so the block body
/// sees the class constructor as its receiver. Captures outer
/// bindings via the normal parent-chain walk; private-name scope
/// is inherited from the enclosing class.
fn synthesise_static_block<'a>(
    outer: &LoweringContext<'a>,
    block: &'a oxc_ast::ast::StaticBlock<'a>,
    class_name: &str,
    class_private_names: std::rc::Rc<[String]>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let params_layout = ParamsLayout {
        names: Vec::new(),
        defaults: Vec::new(),
        patterns: Vec::new(),
        rest_name: None,
        rest_pattern: None,
    };
    let mut builder = BytecodeBuilder::new();
    let mut ctx = LoweringContext::with_parent(
        &params_layout,
        outer.function_names,
        std::rc::Rc::clone(&outer.module_functions),
        Some(outer),
        Some(ClassSuperBinding {
            allow_super_property: true,
            allow_super_call: false,
        }),
        Some(class_private_names),
    );

    let lower = (|| -> Result<(), SourceLoweringError> {
        for stmt in block.body.iter() {
            // `static { ... }` shares the function-body statement
            // surface: `let`/`const` declarations are permitted,
            // expressions / ifs / loops etc. go through the
            // nested path.
            lower_top_statement(&mut builder, &mut ctx, stmt)?;
        }
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
        })?;
        builder
            .emit(Opcode::Return, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
        Ok(())
    })();
    lower?;

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finish static block: {err:?}")))?;
    let bytecode_len = bytecode.bytes().len() as u32;
    let layout = FrameLayout::new(1, 0, ctx.local_count(), ctx.temp_count())
        .map_err(|err| SourceLoweringError::Internal(format!("static block layout: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&ctx.take_feedback_slot_kinds());
    let side_tables = crate::module::FunctionSideTables::new(
        ctx.take_property_names(),
        ctx.take_string_literals(),
        ctx.take_float_constants(),
        ctx.take_bigint_constants(),
        ctx.take_closure_table(bytecode_len),
        Default::default(),
        ctx.take_regexp_literals(),
    );
    let exception_handlers = ctx.take_exception_handlers(&BytecodeBuilder::new())?;
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        crate::exception::ExceptionTable::new(exception_handlers),
        Default::default(),
    );
    let block_name = format!("{class_name}#staticBlock");
    let func = VmFunction::new(Some(block_name), layout, bytecode, tables);
    let captures: Vec<crate::closure::CaptureDescriptor> = ctx
        .captures
        .borrow()
        .iter()
        .map(|entry| entry.descriptor)
        .collect();
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok((idx, captures))
}

fn synthesise_field_initializer<'a>(
    outer: &LoweringContext<'a>,
    fields: &[ClassField<'a>],
    class_name: &str,
    class_private_names: std::rc::Rc<[String]>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let params_layout = ParamsLayout {
        names: Vec::new(),
        defaults: Vec::new(),
        patterns: Vec::new(),
        rest_name: None,
        rest_pattern: None,
    };
    let mut builder = BytecodeBuilder::new();
    let ctx = LoweringContext::with_parent(
        &params_layout,
        outer.function_names,
        std::rc::Rc::clone(&outer.module_functions),
        Some(outer),
        Some(ClassSuperBinding {
            allow_super_property: true,
            allow_super_call: false,
        }),
        Some(class_private_names),
    );

    let this_reg = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::LdaThis, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode LdaThis: {err:?}")))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(this_reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (field init this): {err:?}"))
            })?;
        for field in fields {
            if let Some(init) = field.initializer {
                lower_return_expression(&mut builder, &ctx, init)?;
            } else {
                builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaUndefined (field default): {err:?}"
                    ))
                })?;
            }
            let name_idx = ctx.intern_property_name(&field.name)?;
            let opcode = if field.is_private {
                Opcode::DefinePrivateField
            } else {
                Opcode::DefineField
            };
            builder
                .emit(
                    opcode,
                    &[Operand::Reg(u32::from(this_reg)), Operand::Idx(name_idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode {opcode:?} (field init): {err:?}"
                    ))
                })?;
        }
        builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
        })?;
        builder
            .emit(Opcode::Return, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower?;

    let bytecode = builder.finish().map_err(|err| {
        SourceLoweringError::Internal(format!("finish field initializer: {err:?}"))
    })?;
    let bytecode_len = bytecode.bytes().len() as u32;

    let layout = FrameLayout::new(1, 0, ctx.local_count(), ctx.temp_count())
        .map_err(|err| SourceLoweringError::Internal(format!("field init layout: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&ctx.take_feedback_slot_kinds());
    let side_tables = crate::module::FunctionSideTables::new(
        ctx.take_property_names(),
        ctx.take_string_literals(),
        ctx.take_float_constants(),
        ctx.take_bigint_constants(),
        ctx.take_closure_table(bytecode_len),
        Default::default(),
        ctx.take_regexp_literals(),
    );
    // The field-initializer body can't emit `try`/`catch` — it's
    // compiled from individual expressions, not statements — so
    // the exception handler list is always empty.
    let exception_table = crate::exception::ExceptionTable::new(Vec::new());
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        exception_table,
        Default::default(),
    );
    let init_name = format!("{class_name}#init");
    let func = VmFunction::new(Some(init_name), layout, bytecode, tables);
    let captures: Vec<crate::closure::CaptureDescriptor> = ctx
        .captures
        .borrow()
        .iter()
        .map(|entry| entry.descriptor)
        .collect();
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok((idx, captures))
}

/// §15.7.14 step 10.b — synthesises the default constructor for a
/// derived class: `constructor(...args) { super(...args); }`.
/// Builds the bytecode directly (no AST round-trip) so the
/// synthesised function stays independent of the outer
/// `LoweringContext`'s parameter layout.
///
/// Frame shape: 1 hidden (receiver) + 0 params + 1 local
/// (`r_args` — the rest-args Array) + 1 temp. Bytecode:
///
/// ```text
///   CreateRestParameters                 ; acc = Array(...args)
///   Star r_args                          ; r_args = acc
///   CallSuperSpread RegList{r_args, 1}   ; super(...args), acc = receiver
///   LdaUndefined                         ; §10.2.1.3 derived ctors return
///   Return                               ; undefined → use `this`
/// ```
///
/// The derived-constructor flag is applied by the caller via
/// [`VmFunction::set_derived_constructor`].
fn synthesise_derived_default_constructor<'a>(
    outer: &LoweringContext<'a>,
    class_name: &str,
) -> Result<u32, SourceLoweringError> {
    // 1 hidden + 0 params + 1 local (rest array) + 0 temp. The
    // RegList for CallSuperSpread operates on the local slot
    // directly, so no extra scratch temp is needed.
    let layout = FrameLayout::new(1, 0, 1, 0)
        .map_err(|err| SourceLoweringError::Internal(format!("derived ctor layout: {err:?}")))?;
    // The rest-args array lives at user-visible slot 0. Register
    // operands carry user-visible indices; `read_bytecode_register`
    // adds `hidden_count` at dispatch time, so we must not
    // pre-resolve here.
    let args_reg: RegisterIndex = 0;
    let mut builder = BytecodeBuilder::new();
    builder
        .emit(Opcode::CreateRestParameters, &[])
        .map_err(|err| {
            SourceLoweringError::Internal(format!(
                "encode CreateRestParameters (derived ctor): {err:?}"
            ))
        })?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(args_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (derived ctor args): {err:?}"))
        })?;
    builder
        .emit(
            Opcode::CallSuperSpread,
            &[Operand::RegList {
                base: u32::from(args_reg),
                count: 1,
            }],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode CallSuperSpread (derived ctor): {err:?}"))
        })?;
    builder
        .emit(Opcode::LdaUndefined, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}")))?;
    builder
        .emit(Opcode::Return, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
    let bytecode = builder.finish().map_err(|err| {
        SourceLoweringError::Internal(format!("finish derived default ctor: {err:?}"))
    })?;
    let func = VmFunction::with_empty_tables(Some(class_name.to_owned()), layout, bytecode);
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok(idx)
}

/// Synthesises an empty class constructor function
/// (`function() {}`) as a fresh `VmFunction` and appends it to
/// the shared module list. Returns the new index. Used when a
/// `class` declaration omits an explicit `constructor`.
fn synthesise_empty_constructor<'a>(
    outer: &LoweringContext<'a>,
    class_name: &str,
) -> Result<u32, SourceLoweringError> {
    let layout = FrameLayout::new(1, 0, 0, 0)
        .map_err(|err| SourceLoweringError::Internal(format!("empty ctor layout: {err:?}")))?;
    let mut builder = BytecodeBuilder::new();
    builder
        .emit(Opcode::LdaUndefined, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}")))?;
    builder
        .emit(Opcode::Return, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finish empty ctor: {err:?}")))?;
    let func = VmFunction::with_empty_tables(Some(class_name.to_owned()), layout, bytecode);
    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;
    fns.push(func);
    Ok(idx)
}

/// Lowers `new Foo(args)` — allocates the receiver from
/// `Foo.prototype`, invokes the constructor with
/// `this = receiver` + `new.target = Foo`, and applies the
/// §9.2.2.1 return override (keep explicit object return, fall
/// back to the allocated receiver otherwise).
///
/// Bytecode shape:
///
/// ```text
///   <lower callee>; Star r_callee
///   <lower arg_0>;  Star r_arg0
///   …
///   Construct r_callee, r_callee, RegList{base=r_arg0, count=argc}
/// ```
///
/// `new.target` uses the same register as the target — callers
/// that need a distinct `new.target` would have to be written
/// through class inheritance, which lands with `extends` (M28).
fn lower_new_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a NewExpression<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;
    let has_spread = expr
        .arguments
        .iter()
        .any(|arg| matches!(arg, Argument::SpreadElement(_)));
    if has_spread {
        return lower_new_expression_with_spread(builder, ctx, expr);
    }
    let argc = RegisterIndex::try_from(expr.arguments.len())
        .map_err(|_| SourceLoweringError::Internal("new argument count exceeds u16".into()))?;
    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = if argc == 0 {
        0
    } else {
        ctx.acquire_temps(argc)
            .inspect_err(|_| ctx.release_temps(1))?
    };
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.callee)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (new callee): {err:?}"))
            })?;
        for (offset, arg) in expr.arguments.iter().enumerate() {
            let arg_expr = match arg {
                Argument::SpreadElement(_) => unreachable!("rejected above"),
                other => other.to_expression(),
            };
            lower_return_expression(builder, ctx, arg_expr)?;
            let slot = args_base
                .checked_add(RegisterIndex::try_from(offset).map_err(|_| {
                    SourceLoweringError::Internal("new argument offset overflow".into())
                })?)
                .ok_or_else(|| SourceLoweringError::Internal("new arg slot overflow".into()))?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (new arg): {err:?}"))
                })?;
        }
        builder
            .emit(
                Opcode::Construct,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: u32::from(argc),
                    },
                ],
            )
            .map_err(|err| SourceLoweringError::Internal(format!("encode Construct: {err:?}")))?;
        Ok(())
    })();
    if argc > 0 {
        ctx.release_temps(argc);
    }
    ctx.release_temps(1);
    lower
}

/// Spread-argument `new C(...args)`. Builds a single Array from
/// the spread + plain arguments and dispatches via
/// `ConstructSpread` — the same shape the existing
/// `Construct` path uses, just with the spread arg-window.
fn lower_new_expression_with_spread<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a NewExpression<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;
    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.callee)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (new spread callee): {err:?}"))
            })?;
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateArray (new spread): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(args_base))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (new spread args): {err:?}"))
            })?;
        for arg in expr.arguments.iter() {
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
                                "encode SpreadIntoArray (new): {err:?}"
                            ))
                        })?;
                }
                other => {
                    lower_return_expression(builder, ctx, other.to_expression())?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(args_base))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode ArrayPush (new spread arg): {err:?}"
                            ))
                        })?;
                }
            }
        }
        builder
            .emit(
                Opcode::ConstructSpread,
                &[
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::Reg(u32::from(callee_temp)),
                    Operand::RegList {
                        base: u32::from(args_base),
                        count: 1,
                    },
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode ConstructSpread: {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

/// Recursively lowers a nested `Function` (the body of a
/// `FunctionExpression` or a nested `FunctionDeclaration`) and
/// appends its `VmFunction` to the shared module function list.
/// Returns the assigned `FunctionIndex` as a raw `u32`.
///
/// M25 Phase A: inner functions see an empty outer scope — no
/// captures allowed. Any reference to a name that isn't a
/// local / param / whitelisted global surfaces as
/// `unbound_identifier` from the regular identifier-resolution
/// path. Phase B rewires that branch to synthesise captures.
/// Lowers a nested function and returns `(function_index,
/// captures)`. Captures list drives the parent's
/// `ClosureTemplate` — each entry matches a `LdaUpvalue idx` /
/// `StaUpvalue idx` inside the inner body.
fn lower_inner_function_with_captures<'a>(
    func: &'a Function<'a>,
    outer: &LoweringContext<'a>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let body = func
        .body
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("declared_only_function", func.span))?;
    let name = func.id.as_ref().map(|ident| ident.name.as_str().to_owned());
    lower_inner_callable(outer, &func.params, body, name)
}

/// Shared core for lowering a nested callable (FunctionExpression,
/// ArrowFunctionExpression, or nested FunctionDeclaration). Takes
/// params + body explicitly so the per-AST-shape wrappers can
/// funnel through a single path.
///
/// Allocates a fresh module function index, lowers the body with
/// the outer context as capture parent, produces a `VmFunction`,
/// pushes it to the shared module list, and returns
/// `(idx, captures)` so the caller can record a
/// `ClosureTemplate`.
fn lower_inner_callable<'a>(
    outer: &LoweringContext<'a>,
    params: &'a FormalParameters<'a>,
    body: &'a FunctionBody<'a>,
    name: Option<String>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    lower_inner_callable_with_super(outer, params, body, name, None, None)
}

/// M28/M29 variant of [`lower_inner_callable`] that threads
/// class-scope metadata into the inner function's `LoweringContext`
/// so class methods and constructors can (1) validate `super.x` /
/// `super(args)` uses and (2) resolve `this.#x` / `obj.#x` against
/// the surrounding class's private-name list. Callers outside
/// `lower_class_body_core` always pass `None` for both.
fn lower_inner_callable_with_super<'a>(
    outer: &LoweringContext<'a>,
    params: &'a FormalParameters<'a>,
    body: &'a FunctionBody<'a>,
    name: Option<String>,
    class_super_binding: Option<ClassSuperBinding>,
    class_private_names: Option<std::rc::Rc<[String]>>,
) -> Result<(u32, Vec<crate::closure::CaptureDescriptor>), SourceLoweringError> {
    let params_layout = analyze_params(params)?;
    let param_count = params_layout.param_slot_count();

    let (body_out, captures) = lower_function_body_with_parent(
        body,
        params,
        &params_layout,
        outer.function_names,
        std::rc::Rc::clone(&outer.module_functions),
        Some(outer),
        class_super_binding,
        class_private_names,
    )?;

    let layout = FrameLayout::new(1, param_count, body_out.local_count, body_out.temp_count)
        .map_err(|err| SourceLoweringError::Internal(format!("frame layout invalid: {err:?}")))?;
    let feedback_layout = feedback_layout_from_kinds(&body_out.feedback_slot_kinds);
    let side_tables = crate::module::FunctionSideTables::new(
        body_out.property_names,
        body_out.string_literals,
        body_out.float_constants,
        body_out.bigint_constants,
        body_out.closures,
        Default::default(),
        body_out.regexp_literals,
    );
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        body_out.exceptions,
        body_out.source_map,
    );
    let inner = VmFunction::new(name, layout, body_out.bytecode, tables);

    let mut fns = outer.module_functions.borrow_mut();
    let idx = u32::try_from(fns.len())
        .map_err(|_| SourceLoweringError::Internal("module function index overflow".into()))?;
    fns.push(inner);
    Ok((idx, captures))
}

/// Lowers `!x` / `-x` / `+x` / `~x` / `typeof x` / `void x` into the
/// accumulator.
///
/// Each operator maps to a dedicated single-operand opcode on the
/// accumulator:
/// - `!` → [`Opcode::LogicalNot`] (returns a boolean; works on any
///   value).
/// - `-` → [`Opcode::Negate`] (int32 wraparound on the current
///   source subset).
/// - `+` → [`Opcode::ToNumber`] (identity for int32; coerces other
///   types once the source surface grows).
/// - `~` → [`Opcode::BitwiseNot`] (int32 bitwise NOT).
/// - `typeof` → [`Opcode::TypeOf`].
/// - `void` → evaluate the argument for its side effects, then
///   overwrite acc with `undefined`.
///
/// `delete` is rejected with `unsupported("delete_unary")` — the
/// semantics depend on PropertyAccess / global-binding support that
/// the current source surface hasn't reached yet.
fn lower_unary_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UnaryExpression<'_>,
) -> Result<(), SourceLoweringError> {
    // Evaluate the argument into the accumulator first. The operand
    // lowering already handles every shape
    // `lower_return_expression` accepts, including nested unary /
    // assignment / call expressions, so the operator step below
    // composes cleanly with any int32-producing subexpression.
    lower_return_expression(builder, ctx, &expr.argument)?;

    match expr.operator {
        UnaryOperator::LogicalNot => {
            builder.emit(Opcode::LogicalNot, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LogicalNot: {err:?}"))
            })?;
        }
        UnaryOperator::UnaryNegation => {
            builder
                .emit(Opcode::Negate, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Negate: {err:?}")))?;
        }
        UnaryOperator::UnaryPlus => {
            builder.emit(Opcode::ToNumber, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode ToNumber: {err:?}"))
            })?;
        }
        UnaryOperator::BitwiseNot => {
            builder.emit(Opcode::BitwiseNot, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode BitwiseNot: {err:?}"))
            })?;
        }
        UnaryOperator::Typeof => {
            builder
                .emit(Opcode::TypeOf, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode TypeOf: {err:?}")))?;
        }
        UnaryOperator::Void => {
            // `void x` — evaluate x for side effects (already done
            // above), then discard and return undefined.
            builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaUndefined: {err:?}"))
            })?;
        }
        UnaryOperator::Delete => {
            // §13.5.1 The `delete` Operator. For property accesses
            // we route through `DelNamedProperty` / `DelKeyedProperty`
            // opcodes. For bare-identifier deletes (`delete x` in
            // sloppy mode) JS returns `true` but removes only when
            // `x` is a configurable global — we conservatively
            // surface `true` without any side effect to match the
            // most common test262 cases; actual global removal
            // can land with S1's capability story.
            // Note: we lowered the argument above (for side effects
            // + simple-reference cases); here we emit the delete
            // against the right target.
            match &expr.argument {
                Expression::StaticMemberExpression(member) => {
                    let target_temp = ctx.acquire_temps(1)?;
                    let lower = (|| -> Result<(), SourceLoweringError> {
                        lower_return_expression(builder, ctx, &member.object)?;
                        builder
                            .emit(Opcode::Star, &[Operand::Reg(u32::from(target_temp))])
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode Star (delete target): {err:?}"
                                ))
                            })?;
                        let idx = ctx.intern_property_name(member.property.name.as_str())?;
                        builder
                            .emit(
                                Opcode::DelNamedProperty,
                                &[Operand::Reg(u32::from(target_temp)), Operand::Idx(idx)],
                            )
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode DelNamedProperty: {err:?}"
                                ))
                            })?;
                        Ok(())
                    })();
                    ctx.release_temps(1);
                    lower?;
                }
                Expression::ComputedMemberExpression(member) => {
                    let target_temp = ctx.acquire_temps(1)?;
                    let lower = (|| -> Result<(), SourceLoweringError> {
                        lower_return_expression(builder, ctx, &member.object)?;
                        builder
                            .emit(Opcode::Star, &[Operand::Reg(u32::from(target_temp))])
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode Star (delete keyed target): {err:?}"
                                ))
                            })?;
                        lower_return_expression(builder, ctx, &member.expression)?;
                        builder
                            .emit(
                                Opcode::DelKeyedProperty,
                                &[Operand::Reg(u32::from(target_temp))],
                            )
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode DelKeyedProperty: {err:?}"
                                ))
                            })?;
                        Ok(())
                    })();
                    ctx.release_temps(1);
                    lower?;
                }
                _ => {
                    // `delete expr` on a non-reference returns `true`
                    // per §13.5.1 step 3.
                    builder.emit(Opcode::LdaTrue, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaTrue (delete non-reference): {err:?}"
                        ))
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Lowers `++x` / `x++` / `--x` / `x--` onto a writable local
/// binding.
///
/// Prefix form (`++x`) bytecode shape:
///
/// ```text
///   Ldar r_x         ; acc = old x
///   Inc              ; acc = old + 1
///   Star r_x         ; x = new value (also in acc for composition)
/// ```
///
/// Postfix form (`x++`) bytecode shape:
///
/// ```text
///   Ldar r_x         ; acc = old x
///   Star r_temp      ; temp = old (preserved for the expression's value)
///   Inc              ; acc = old + 1
///   Star r_x         ; x = new value
///   Ldar r_temp      ; acc = old (the expression result)
/// ```
///
/// The int32 envelope means `ToNumber` coercion is implicit: the
/// operand is int32 throughout, so `Inc`/`Dec` produces int32 with
/// wraparound semantics that match `x + 1 | 0` / `x - 1 | 0`. A
/// future milestone that grows past int32 will need an explicit
/// `ToNumber` step to preserve JS postfix semantics ("return the
/// coerced number, write the incremented value").
///
/// Rejects:
/// - non-identifier target → `non_identifier_update_target`;
/// - unbound identifier → `unbound_identifier`;
/// - parameter as target → `update_on_param`;
/// - `const` binding as target → `const_update`;
/// - in-TDZ binding → `tdz_self_reference`.
fn lower_update_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &UpdateExpression<'_>,
) -> Result<(), SourceLoweringError> {
    // 1) Target must be a plain identifier; anything else (member,
    //    computed, TS-only) is out of scope for M10.
    let ident = match &expr.argument {
        SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => ident.as_ref(),
        _ => {
            return Err(SourceLoweringError::unsupported(
                "non_identifier_update_target",
                expr.span,
            ));
        }
    };
    let binding = ctx
        .resolve_identifier(ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::unsupported("unbound_identifier", ident.span))?;
    let target_reg = match binding {
        BindingRef::Local {
            reg,
            initialized: true,
            is_const: false,
            ..
        } => reg,
        BindingRef::Local { is_const: true, .. } => {
            return Err(SourceLoweringError::unsupported("const_update", ident.span));
        }
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident.span,
            ));
        }
        // `x++` / `++x` on a parameter: parameters live in
        // their own register window just like `let` bindings do,
        // so incrementing writes back to the same slot with no
        // observable aliasing (spec §14.1.21 treats parameters
        // as mutable bindings by default).
        BindingRef::Param { reg } => reg,
        BindingRef::Upvalue { .. } => {
            return Err(SourceLoweringError::unsupported(
                "update_on_upvalue",
                ident.span,
            ));
        }
    };

    let op_opcode = match expr.operator {
        UpdateOperator::Increment => Opcode::Inc,
        UpdateOperator::Decrement => Opcode::Dec,
    };
    let op_label = match expr.operator {
        UpdateOperator::Increment => "Inc",
        UpdateOperator::Decrement => "Dec",
    };

    // 2) Load old value into acc. Reuses `lower_identifier_read` so
    //    the emitted `Ldar` also picks up a fresh arithmetic feedback
    //    slot for M_JIT_C.2 / M_JIT_C.3 consumption.
    lower_identifier_read(builder, ctx, binding, ident.span)?;

    if expr.prefix {
        // Prefix: Inc/Dec in place, Star back.
        builder
            .emit(op_opcode, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {op_label}: {err:?}")))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(target_reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (prefix update): {err:?}"))
            })?;
    } else {
        // Postfix: spill old to a temp, Inc/Dec, Star back, reload
        // the spilled old value into acc so the expression's value
        // is the pre-increment int32. The temp is released once we
        // reload, matching the LIFO contract callers rely on for
        // nested calls.
        let temp = ctx.acquire_temps(1)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (postfix old-value spill): {err:?}"
                ))
            })
            .inspect_err(|_| ctx.release_temps(1))?;
        builder
            .emit(op_opcode, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {op_label}: {err:?}")))
            .inspect_err(|_| ctx.release_temps(1))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(target_reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (postfix update): {err:?}"))
            })
            .inspect_err(|_| ctx.release_temps(1))?;
        // Reload old value. No feedback slot attached — this is a
        // purely mechanical temp reload, not a user-facing read.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (postfix old reload): {err:?}"))
            })
            .inspect_err(|_| ctx.release_temps(1))?;
        ctx.release_temps(1);
    }
    Ok(())
}

/// Lowers `test ? consequent : alternate` (ConditionalExpression).
///
/// Bytecode shape — the standard branch-and-join:
///
/// ```text
///   <lower test>                ; acc = test
///   JumpIfToBooleanFalse else_label
///   <lower consequent>          ; acc = consequent
///   Jump end_label
/// else_label:
///   <lower alternate>           ; acc = alternate
/// end_label:
/// ```
///
/// `JumpIfToBooleanFalse` takes the ToBoolean coercion path the
/// interpreter already performs for `if` / `while` conditions, so
/// any truthy-or-falsy JS value works as the test — not just a
/// strict boolean. Result lands in the accumulator ready for
/// composition with surrounding expressions.
fn lower_conditional_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ConditionalExpression<'_>,
) -> Result<(), SourceLoweringError> {
    let else_label = builder.new_label();
    let end_label = builder.new_label();

    lower_return_expression(builder, ctx, &expr.test)?;
    builder
        .emit_jump_to(Opcode::JumpIfToBooleanFalse, else_label)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfToBooleanFalse (ternary): {err:?}"))
        })?;
    lower_return_expression(builder, ctx, &expr.consequent)?;
    builder
        .emit_jump_to(Opcode::Jump, end_label)
        .map_err(|err| SourceLoweringError::Internal(format!("encode Jump (ternary): {err:?}")))?;
    builder
        .bind_label(else_label)
        .map_err(|err| SourceLoweringError::Internal(format!("bind ternary else: {err:?}")))?;
    lower_return_expression(builder, ctx, &expr.alternate)?;
    builder
        .bind_label(end_label)
        .map_err(|err| SourceLoweringError::Internal(format!("bind ternary end: {err:?}")))?;
    Ok(())
}

/// Lowers `a && b` / `a || b` / `a ?? b` with the spec-mandated
/// short-circuit semantics.
///
/// `&&` returns `a` if `a` is falsy (ToBoolean false), else `b`.
/// `||` returns `a` if `a` is truthy (ToBoolean true), else `b`.
/// `??` returns `a` if `a` is **neither** `null` nor `undefined`,
/// else `b`. None of the operators coerce the surviving left-hand
/// value — `0 && x` returns `0` (not `false`), `"" || x` returns
/// `x` (after the truthy test on `""` sees falsy), and `null ?? x`
/// returns `x`.
///
/// Bytecode shape (for `&&`, showing the representative
/// branch-and-join):
///
/// ```text
///   <lower left>                  ; acc = left
///   JumpIfToBooleanFalse end      ; short-circuit: keep acc = left
///   <lower right>                 ; acc = right
/// end:
/// ```
///
/// `||` uses `JumpIfToBooleanTrue` instead. `??` uses a two-step
/// `JumpIfNotNull` + `JumpIfNotUndefined` sequence so the short-
/// circuit only kicks in when `left` is not null/undefined.
fn lower_logical_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &LogicalExpression<'_>,
) -> Result<(), SourceLoweringError> {
    lower_return_expression(builder, ctx, &expr.left)?;

    match expr.operator {
        LogicalOperator::And => {
            let end_label = builder.new_label();
            builder
                .emit_jump_to(Opcode::JumpIfToBooleanFalse, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfToBooleanFalse (&&): {err:?}"
                    ))
                })?;
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .bind_label(end_label)
                .map_err(|err| SourceLoweringError::Internal(format!("bind &&: {err:?}")))?;
        }
        LogicalOperator::Or => {
            let end_label = builder.new_label();
            builder
                .emit_jump_to(Opcode::JumpIfToBooleanTrue, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfToBooleanTrue (||): {err:?}"
                    ))
                })?;
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .bind_label(end_label)
                .map_err(|err| SourceLoweringError::Internal(format!("bind ||: {err:?}")))?;
        }
        LogicalOperator::Coalesce => {
            // `a ?? b`: short-circuit to `end` when `a` is neither
            // null nor undefined. Otherwise fall through to the
            // right-hand lowering. The two-step probe exploits the
            // existing `JumpIfNotNull` / `JumpIfNotUndefined`
            // opcodes without introducing a new "is nullish" op.
            //
            // Control flow:
            //   acc = a
            //   if acc != null → jump check_undefined
            //   // acc == null: fall through to lower b
            //   <lower b>
            //   jump end
            //   check_undefined:
            //   if acc != undefined → jump end (keep acc = a)
            //   <lower b>   [reached only when acc was undefined]
            //   end:
            //
            // The block below emits a simpler equivalent by sharing
            // the right-hand lowering for both the null and
            // undefined cases — a single `lower_right` block is
            // used regardless of which nullish value matched.
            let check_undefined = builder.new_label();
            let lower_right_label = builder.new_label();
            let end_label = builder.new_label();
            builder
                .emit_jump_to(Opcode::JumpIfNotNull, check_undefined)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode JumpIfNotNull (??): {err:?}"))
                })?;
            // `a` is null — fall through to the right-hand path.
            builder
                .emit_jump_to(Opcode::Jump, lower_right_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Jump (?? null → right): {err:?}"))
                })?;
            builder.bind_label(check_undefined).map_err(|err| {
                SourceLoweringError::Internal(format!("bind ?? check_undefined: {err:?}"))
            })?;
            // Not null — check undefined. If not undefined either,
            // short-circuit to end keeping `acc = a`.
            builder
                .emit_jump_to(Opcode::JumpIfNotUndefined, end_label)
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode JumpIfNotUndefined (??): {err:?}"
                    ))
                })?;
            builder.bind_label(lower_right_label).map_err(|err| {
                SourceLoweringError::Internal(format!("bind ?? lower_right: {err:?}"))
            })?;
            lower_return_expression(builder, ctx, &expr.right)?;
            builder
                .bind_label(end_label)
                .map_err(|err| SourceLoweringError::Internal(format!("bind ?? end: {err:?}")))?;
        }
    }
    Ok(())
}

/// Lowers an `ObjectExpression` literal with static-identifier or
/// string-literal keys. Computed keys, methods, shorthand, spread,
/// getters, and setters are rejected with a stable per-shape tag —
/// later milestones widen the surface.
///
/// Bytecode shape:
///
/// ```text
///   CreateObject               ; acc = {}
///   Star r_obj                 ; spill object handle to a temp
///   <lower value_0>            ; acc = value_0
///   StaNamedProperty r_obj, k0 ; obj[k0] = value_0
///   <lower value_1>            ; acc = value_1
///   StaNamedProperty r_obj, k1 ; obj[k1] = value_1
///   …
///   Ldar r_obj                 ; acc = obj (result of the expression)
/// ```
///
/// The empty-object case `{}` collapses to a single `CreateObject`
/// with no temp-slot traffic — neither the spill nor the reload are
/// emitted.
///
/// §13.2.5 Object Initializer
/// <https://tc39.es/ecma262/#sec-object-initializer>
fn lower_object_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ObjectExpression<'_>,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::CreateObject, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateObject: {err:?}")))?;

    if expr.properties.is_empty() {
        return Ok(());
    }

    // Acquire a temp to hold the object handle across the property
    // initialisers — each value lowering clobbers acc.
    let obj_temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(obj_temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (object temp): {err:?}"))
        })?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        for prop_kind in &expr.properties {
            let prop = match prop_kind {
                ObjectPropertyKind::ObjectProperty(p) => p,
                // `{ ...source }` — spread. Evaluate `source`,
                // then copy every own enumerable property onto
                // the target via `CopyDataProperties` (runtime
                // helper).
                ObjectPropertyKind::SpreadProperty(s) => {
                    lower_return_expression(builder, ctx, &s.argument)?;
                    builder
                        .emit(
                            Opcode::CopyDataProperties,
                            &[Operand::Reg(u32::from(obj_temp))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode CopyDataProperties: {err:?}"
                            ))
                        })?;
                    continue;
                }
            };
            // Accessor property (`{ get x() {} }` / `{ set x(v) {} }`).
            // Lower the value (a FunctionExpression) into acc,
            // then emit DefineClassGetter / DefineClassSetter
            // — the class-accessor opcode installs the closure
            // as an accessor-half on the target. Class methods
            // use `enumerable=false`; object-literal accessors
            // are spec'd `enumerable=true`, a small divergence
            // invisible outside `Object.keys` / `for...in`.
            if !matches!(prop.kind, PropertyKind::Init) {
                let is_getter = matches!(prop.kind, PropertyKind::Get);
                if prop.computed {
                    let key_temp = ctx.acquire_temps(1)?;
                    let comp_result = (|| -> Result<(), SourceLoweringError> {
                        lower_return_expression(builder, ctx, prop.key.to_expression())?;
                        builder
                            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode Star (accessor computed key): {err:?}"
                                ))
                            })?;
                        lower_return_expression(builder, ctx, &prop.value)?;
                        let accessor_opcode = if is_getter {
                            Opcode::DefineClassGetterComputed
                        } else {
                            Opcode::DefineClassSetterComputed
                        };
                        builder
                            .emit(
                                accessor_opcode,
                                &[
                                    Operand::Reg(u32::from(obj_temp)),
                                    Operand::Reg(u32::from(key_temp)),
                                ],
                            )
                            .map_err(|err| {
                                SourceLoweringError::Internal(format!(
                                    "encode accessor computed: {err:?}"
                                ))
                            })?;
                        Ok(())
                    })();
                    ctx.release_temps(1);
                    comp_result?;
                    continue;
                }
                let key_name = match &prop.key {
                    PropertyKey::StaticIdentifier(ident) => ident.name.as_str().to_owned(),
                    PropertyKey::StringLiteral(lit) => lit.value.as_str().to_owned(),
                    other => {
                        return Err(SourceLoweringError::unsupported(
                            property_key_tag(other),
                            other.span(),
                        ));
                    }
                };
                let idx = ctx.intern_property_name(&key_name)?;
                lower_return_expression(builder, ctx, &prop.value)?;
                let accessor_opcode = if is_getter {
                    Opcode::DefineClassGetter
                } else {
                    Opcode::DefineClassSetter
                };
                builder
                    .emit(
                        accessor_opcode,
                        &[Operand::Reg(u32::from(obj_temp)), Operand::Idx(idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode accessor: {err:?}"))
                    })?;
                continue;
            }
            // Computed key: `{ [expr]: value }`. Lower the key
            // expression into a temp, then use `StaKeyedProperty`
            // so the runtime handles the ToPropertyKey + set.
            if prop.computed {
                let key_temp = ctx.acquire_temps(1)?;
                let computed_lower = (|| -> Result<(), SourceLoweringError> {
                    lower_return_expression(builder, ctx, prop.key.to_expression())?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (obj computed key): {err:?}"
                            ))
                        })?;
                    lower_return_expression(builder, ctx, &prop.value)?;
                    builder
                        .emit(
                            Opcode::StaKeyedProperty,
                            &[
                                Operand::Reg(u32::from(obj_temp)),
                                Operand::Reg(u32::from(key_temp)),
                            ],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode StaKeyedProperty (obj computed): {err:?}"
                            ))
                        })?;
                    Ok(())
                })();
                ctx.release_temps(1);
                computed_lower?;
                continue;
            }
            let key_name = match &prop.key {
                PropertyKey::StaticIdentifier(ident) => ident.name.as_str().to_owned(),
                PropertyKey::StringLiteral(lit) => lit.value.as_str().to_owned(),
                other => {
                    return Err(SourceLoweringError::unsupported(
                        property_key_tag(other),
                        other.span(),
                    ));
                }
            };
            // Lower the value into acc. `{ x }` (shorthand) and
            // `{ foo() {} }` (method) both have their value
            // correctly modelled by oxc: shorthand's value is an
            // Identifier reference, method's value is a
            // FunctionExpression — both already supported by
            // `lower_return_expression`. No special case
            // required.
            lower_return_expression(builder, ctx, &prop.value)?;
            let idx = ctx.intern_property_name(&key_name)?;
            builder
                .emit(
                    Opcode::StaNamedProperty,
                    &[Operand::Reg(u32::from(obj_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode StaNamedProperty: {err:?}"))
                })?;
        }
        // Reload the object handle so the expression's value is in
        // acc for the caller.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(obj_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (object reload): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Lowers an `ArrayExpression` literal. Elements are emitted in
/// source order via `ArrayPush` — the runtime's array helper bumps
/// `length` and writes into the dense elements slot. Spread elements
/// and holes (`[1, , 2]`) are rejected with a stable tag so future
/// milestones can widen the surface without silently changing
/// semantics.
///
/// Bytecode shape:
///
/// ```text
///   CreateArray                ; acc = []
///   Star r_arr
///   <lower element_0>          ; acc = element_0
///   ArrayPush r_arr            ; arr.push(element_0)
///   <lower element_1>
///   ArrayPush r_arr
///   …
///   Ldar r_arr                 ; acc = arr
/// ```
///
/// The empty-array case `[]` collapses to a single `CreateArray`
/// with no temp traffic.
///
/// §13.2.4 Array Initializer
/// <https://tc39.es/ecma262/#sec-array-initializer>
fn lower_array_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ArrayExpression<'_>,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::CreateArray, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode CreateArray: {err:?}")))?;

    if expr.elements.is_empty() {
        return Ok(());
    }

    let arr_temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(arr_temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (array temp): {err:?}"))
        })?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        for element in &expr.elements {
            match element {
                ArrayExpressionElement::SpreadElement(spread) => {
                    // M23: `[...iter]` — iterate the spread
                    // source and push each value. The
                    // `SpreadIntoArray r_arr` opcode handles the
                    // iterator protocol + push loop in the
                    // dispatcher; here we just lower the source
                    // into acc and emit the opcode.
                    lower_return_expression(builder, ctx, &spread.argument)?;
                    builder
                        .emit(
                            Opcode::SpreadIntoArray,
                            &[Operand::Reg(u32::from(arr_temp))],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode SpreadIntoArray (array literal): {err:?}"
                            ))
                        })?;
                }
                ArrayExpressionElement::Elision(_elision) => {
                    // `[1, , 3]` — a hole creates a sparse slot
                    // whose length counts it but whose indexed
                    // access returns `undefined` and whose `in`
                    // check returns `false`. Simulate by pushing
                    // `undefined`; the resulting array is dense
                    // but indistinguishable for the vast majority
                    // of user code. True holes need an
                    // `ArrayPushHole` opcode that doesn't exist
                    // yet — follow-up work.
                    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaUndefined (elision): {err:?}"
                        ))
                    })?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(arr_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode ArrayPush (elision): {err:?}"
                            ))
                        })?;
                }
                // Non-spread, non-hole element. `to_expression`
                // downcasts the `Expression` variants inlined by
                // `ArrayExpressionElement` back to `&Expression`.
                other => {
                    let element_expr = other.to_expression();
                    lower_return_expression(builder, ctx, element_expr)?;
                    builder
                        .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(arr_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!("encode ArrayPush: {err:?}"))
                        })?;
                }
            }
        }
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(arr_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (array reload): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Materialises the base object of a member expression into a
/// register that the caller can feed to `Lda*Property` /
/// `Sta*Property`. Fast path: if the base is an in-scope identifier
/// bound to a parameter or initialised local, its slot is returned
/// directly and no temp is acquired. Otherwise the base is lowered
/// into the accumulator and spilled into a freshly-acquired temp
/// slot; the caller must call `release_temps(temp_count)` in LIFO
/// order once the emitted opcode consuming the base has run.
///
/// `temp_count` is always 0 or 1 and tells the caller whether to
/// release a slot.
struct MemberBase {
    reg: RegisterIndex,
    temp_count: RegisterIndex,
}

fn materialize_member_base<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    base: &'a Expression<'a>,
) -> Result<MemberBase, SourceLoweringError> {
    if let Expression::Identifier(ident) = base
        && let Some(binding) = ctx.resolve_identifier(ident.name.as_str())
    {
        match binding {
            BindingRef::Param { reg } => return Ok(MemberBase { reg, temp_count: 0 }),
            BindingRef::Local {
                reg,
                initialized: true,
                ..
            } => return Ok(MemberBase { reg, temp_count: 0 }),
            BindingRef::Local {
                initialized: false, ..
            } => {
                return Err(SourceLoweringError::unsupported(
                    "tdz_self_reference",
                    ident.span,
                ));
            }
            // Upvalue base: no dedicated register, so fall
            // through to the complex-path below (lower into acc,
            // spill to a temp).
            BindingRef::Upvalue { .. } => {}
        }
    }

    // Complex / non-local base — lower into acc and spill to a temp.
    lower_return_expression(builder, ctx, base)?;
    let temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (member base spill): {err:?}"))
        })?;
    Ok(MemberBase {
        reg: temp,
        temp_count: 1,
    })
}

/// Lowers `o.x` into the accumulator. Base goes through
/// [`materialize_member_base`] (direct-reg fast path for identifier
/// bases, temp-spill for everything else); the property name is
/// interned into the function's `PropertyNameTable` with dedup.
///
/// Optional chaining (`o?.x`) is handled via a nullish short-circuit
/// jump: the caller — [`lower_chain_expression`] — pushes the
/// chain's short-circuit label onto the context stack before
/// lowering the chain's inner expression. When this helper sees
/// `expr.optional == true` and finds an active short-circuit label
/// on the stack, it emits a `JumpIfNull` / `JumpIfUndefined` pair
/// against the materialised base object before the property load.
/// `o?.x` outside any chain is a parser / AST invariant violation
/// and stays rejected defensively.
///
/// §13.3.9 Optional Chains
/// <https://tc39.es/ecma262/#sec-optional-chains>
/// §13.3.2 Property Accessors
/// <https://tc39.es/ecma262/#sec-property-accessors>
fn lower_static_member_read(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &StaticMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = if expr.optional {
        let Some(short_circuit) = ctx.optional_chain_short_circuit() else {
            return Err(SourceLoweringError::unsupported(
                "optional_member_expression",
                expr.span,
            ));
        };
        Some(short_circuit)
    } else {
        None
    };
    // M28: `super.x` — §13.3.7 SuperReference. Uses the enclosing
    // method's `[[HomeObject]]` (resolved at runtime inside the
    // `GetSuperProperty` opcode) as the lookup base, and the
    // current frame's `this` as the `[[Get]]` receiver.
    if matches!(&expr.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &expr.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super.x): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super.x receiver): {err:?}"
                    ))
                })?;
            let idx = ctx.intern_property_name(expr.property.name.as_str())?;
            builder
                .emit(
                    Opcode::GetSuperProperty,
                    &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode GetSuperProperty: {err:?}"))
                })?;
            Ok(())
        })();
        ctx.release_temps(1);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &expr.object)?;
    if let Some(short_circuit) = optional_short_circuit {
        emit_optional_nullish_short_circuit(builder, base.reg, short_circuit)?;
    }
    let idx = ctx.intern_property_name(expr.property.name.as_str())?;
    // P1: attach a property-feedback slot so the dispatcher can
    // probe the cached `(shape_id, slot_index)` for this PC on
    // subsequent executions. On first hit the slot transitions
    // `Uninitialized → Monomorphic`; diverging shapes bump it to
    // `Polymorphic` (up to 4); beyond that it pins `Megamorphic`
    // and always takes the slow path.
    let pc = builder
        .emit(
            Opcode::LdaNamedProperty,
            &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaNamedProperty: {err:?}"))
        })?;
    let slot = ctx.allocate_property_feedback();
    builder.attach_feedback(pc, slot);
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    Ok(())
}

/// Emits the nullish short-circuit sequence for an optional member
/// / call access. `base_reg` holds the object or callee value;
/// when it's `null` or `undefined` control jumps to `short_circuit`
/// (where the chain lowerer has arranged for `undefined` to be
/// loaded into the accumulator). Two jumps beats a single
/// `TestUndetectable + JumpIfToBooleanTrue` pair in the common
/// non-null case — both JumpIfNull/JumpIfUndefined are single-byte
/// tagged tests followed by a 4-byte jump operand with no
/// boolean-coercion step.
fn emit_optional_nullish_short_circuit(
    builder: &mut BytecodeBuilder,
    base_reg: RegisterIndex,
    short_circuit: Label,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(base_reg))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (optional chain base): {err:?}"))
        })?;
    builder
        .emit_jump_to(Opcode::JumpIfNull, short_circuit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfNull (optional): {err:?}"))
        })?;
    builder
        .emit_jump_to(Opcode::JumpIfUndefined, short_circuit)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode JumpIfUndefined (optional): {err:?}"))
        })?;
    Ok(())
}

/// Lowers `o[k]` into the accumulator. Shape:
///
/// ```text
///   <materialize base into r_base>
///   <lower key into acc>
///   LdaKeyedProperty r_base     ; acc = r_base[acc]
/// ```
///
/// Optional chaining rejected.
///
/// §13.3.2 Property Accessors
/// <https://tc39.es/ecma262/#sec-property-accessors>
fn lower_computed_member_read(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &ComputedMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    let optional_short_circuit = if expr.optional {
        let Some(short_circuit) = ctx.optional_chain_short_circuit() else {
            return Err(SourceLoweringError::unsupported(
                "optional_member_expression",
                expr.span,
            ));
        };
        Some(short_circuit)
    } else {
        None
    };
    // M28: `super[k]` — dynamic-key super property read. Receiver
    // is `this`; key is evaluated into a dedicated temp so the
    // `GetSuperPropertyComputed` operand shape `(Reg, Reg)` matches.
    if matches!(&expr.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &expr.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super[k]): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super[k] receiver): {err:?}"
                    ))
                })?;
            lower_return_expression(builder, ctx, &expr.expression)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (super[k] key): {err:?}"))
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
        ctx.release_temps(2);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &expr.object)?;
    if let Some(short_circuit) = optional_short_circuit {
        emit_optional_nullish_short_circuit(builder, base.reg, short_circuit)?;
    }
    lower_return_expression(builder, ctx, &expr.expression)?;
    builder
        .emit(
            Opcode::LdaKeyedProperty,
            &[Operand::Reg(u32::from(base.reg))],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaKeyedProperty: {err:?}"))
        })?;
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    Ok(())
}

/// Lowers a template literal (`` `hello` ``, `` `hi, ${name}` ``, …)
/// into a running string concatenation. Tagged templates
/// (`` tag`…` ``) are a separate AST node
/// (`TaggedTemplateExpression`) and aren't accepted here — they need
/// the full tag-call protocol and the raw-strings array, neither of
/// which the current source surface supports.
///
/// Shape with N substitutions (quasis = `[q0, q1, …, qN]`,
/// expressions = `[e0, …, e_{N-1}]`, so the logical sequence is
/// `q0 ++ e0 ++ q1 ++ e1 ++ … ++ q_{N-1} ++ e_{N-1} ++ qN`):
///
/// Simple form (`N = 0`, single quasi, no substitutions):
///
/// ```text
///   LdaConstStr q0_idx
/// ```
///
/// Interpolated form — the compiler keeps a running "buffer" temp
/// (`r_buf`) plus a scratch temp (`r_tmp`) so each concat step stays
/// LHS-first (string `+` is non-commutative):
///
/// ```text
///   LdaConstStr q0_idx         ; acc = q0
///   Star r_buf                 ; r_buf = q0
///   ; for each piece (expression e_i, then quasi q_{i+1} unless empty):
///   <lower e_i into acc>
///   Star r_tmp                 ; r_tmp = piece
///   Ldar r_buf                 ; acc = r_buf
///   Add r_tmp                  ; acc = r_buf + piece  (string concat)
///   Star r_buf                 ; roll the buffer forward
///   ; last piece leaves the result in acc without a trailing Star.
/// ```
///
/// Empty non-head quasis (`` `${a}` ``'s final `""`, `` `a${x}b${y}` ``'s
/// head `""` if the literal started with a substitution) are skipped
/// — they're semantically a no-op concat and the Add is unnecessary.
/// Empty `cooked` (invalid escape) is rejected with
/// `invalid_template_escape`.
///
/// §13.2.8 Template Literals
/// <https://tc39.es/ecma262/#sec-template-literals>
fn lower_template_literal(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    tpl: &TemplateLiteral<'_>,
) -> Result<(), SourceLoweringError> {
    // Expressions.len() == quasis.len() - 1 by construction.
    if tpl.quasis.len() != tpl.expressions.len() + 1 {
        return Err(SourceLoweringError::Internal(format!(
            "template literal has {} quasis for {} expressions",
            tpl.quasis.len(),
            tpl.expressions.len()
        )));
    }

    let quasi_cooked = |index: usize| -> Result<&str, SourceLoweringError> {
        let q = &tpl.quasis[index];
        match q.value.cooked.as_deref() {
            Some(s) => Ok(s),
            None => Err(SourceLoweringError::unsupported(
                "invalid_template_escape",
                q.span,
            )),
        }
    };

    // No substitutions → just emit the head quasi. This covers the
    // simple form `` `hello` `` and the empty form `` `` ``.
    if tpl.expressions.is_empty() {
        let text = quasi_cooked(0)?;
        let idx = ctx.intern_string_literal(text)?;
        builder
            .emit(Opcode::LdaConstStr, &[Operand::Idx(idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaConstStr (template): {err:?}"))
            })?;
        return Ok(());
    }

    // Interpolated form. Keep a running result in `r_buf` and use
    // `r_tmp` to hold each fresh piece before the `Add r_tmp`.
    let buf = ctx.acquire_temps(1)?;
    let tmp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // 1) Load quasi[0] into acc, spill to r_buf. Using the head
        //    as the starting value keeps the concat LHS-first for
        //    the first substitution — critical since every later
        //    `Add r_tmp` computes `acc + r_tmp`, which must equal
        //    `buf + piece` in that order.
        let head = quasi_cooked(0)?;
        let head_idx = ctx.intern_string_literal(head)?;
        builder
            .emit(Opcode::LdaConstStr, &[Operand::Idx(head_idx)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaConstStr (head): {err:?}"))
            })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(buf))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (template buf): {err:?}"))
            })?;

        // 2) Walk the pieces: for each expression `e_i` emit
        //    `<lower e_i>; Star r_tmp; Ldar r_buf; Add r_tmp;`. Then
        //    (if the following quasi is non-empty) do the same for
        //    `q_{i+1}`. After each concat, roll the buffer forward
        //    via `Star r_buf` — except after the very last piece,
        //    where we leave the result in acc for the caller.
        let last_expr = tpl.expressions.len() - 1;

        for (i, expr) in tpl.expressions.iter().enumerate() {
            let next_quasi_text = quasi_cooked(i + 1)?;
            let has_next_quasi = !next_quasi_text.is_empty();
            let is_last_piece_overall = i == last_expr && !has_next_quasi;

            // Append `expr` to `r_buf`.
            lower_return_expression(builder, ctx, expr)?;
            concat_step(builder, ctx, tmp, buf)?;

            if is_last_piece_overall {
                // Skip the trailing `Star r_buf` — acc already holds
                // the final running result.
                continue;
            }
            // Roll buffer forward so the next piece concatenates
            // against the fresh value.
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(buf))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (template buf roll): {err:?}"
                    ))
                })?;

            if has_next_quasi {
                let quasi_idx = ctx.intern_string_literal(next_quasi_text)?;
                builder
                    .emit(Opcode::LdaConstStr, &[Operand::Idx(quasi_idx)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaConstStr (template quasi): {err:?}"
                        ))
                    })?;
                concat_step(builder, ctx, tmp, buf)?;
                if i != last_expr {
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(buf))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (template buf roll 2): {err:?}"
                            ))
                        })?;
                }
            }
        }
        Ok(())
    })();
    ctx.release_temps(1); // tmp
    ctx.release_temps(1); // buf
    lower
}

/// Emits `Star r_tmp; Ldar r_buf; Add r_tmp` to append the value
/// currently in the accumulator onto the running buffer in `r_buf`.
/// Result ends up in acc (`r_buf + piece`). Attaches an arithmetic
/// feedback slot to the `Add` so JIT baseline recompiles see the
/// path as observed — the value will always be `Any` (string
/// concat), which keeps the tag guards in place.
fn concat_step(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    tmp: RegisterIndex,
    buf: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(tmp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (template tmp): {err:?}"))
        })?;
    builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(buf))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Ldar (template buf): {err:?}"))
        })?;
    let add_pc = builder
        .emit(Opcode::Add, &[Operand::Reg(u32::from(tmp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Add (template concat): {err:?}"))
        })?;
    let slot = ctx.allocate_arithmetic_feedback();
    builder.attach_feedback(add_pc, slot);
    Ok(())
}

/// §13.3.11 `` tag`quasi0${e0}quasi1…` `` — lowers a tagged
/// template call into `tag(strings, e0, e1, …)` where `strings`
/// is the cooked-parts array with a `.raw` property pointing at
/// the raw-parts array.
///
/// Bytecode shape (`N` = substitution count):
///
/// ```text
///   <lower tag>; Star r_callee
///   CreateArray; Star r_args[0]          ; strings (cooked)
///   <for each cooked>: LdaConstStr; ArrayPush r_args[0]
///   CreateArray; Star r_raw              ; raw array
///   <for each raw>: LdaConstStr; ArrayPush r_raw
///   Ldar r_raw; StaNamedProperty r_args[0], "raw"_idx
///   <lower e0>; Star r_args[1]
///   …
///   <lower eN>; Star r_args[N]
///   CallUndefinedReceiver r_callee, RegList { base: r_args, count: N + 1 }
/// ```
///
/// Departs from the spec in one place: §13.2.8.3 / §13.2.8.4
/// require that the cooked and raw arrays be frozen and cached
/// per template-site across invocations. A fresh array is built
/// on every call — observable only via
/// `template === sameTemplateFn()` identity tests, which aren't
/// in the common path.
fn lower_tagged_template_expression<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    tagged: &'a oxc_ast::ast::TaggedTemplateExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let tpl = &tagged.quasi;
    if tpl.quasis.len() != tpl.expressions.len() + 1 {
        return Err(SourceLoweringError::Internal(format!(
            "tagged template has {} quasis for {} expressions",
            tpl.quasis.len(),
            tpl.expressions.len(),
        )));
    }

    let argc = RegisterIndex::try_from(tpl.expressions.len() + 1)
        .map_err(|_| SourceLoweringError::Internal("tagged template argc overflow".into()))?;

    let callee_temp = ctx.acquire_temps(1)?;
    let args_base = ctx
        .acquire_temps(argc)
        .inspect_err(|_| ctx.release_temps(1))?;
    let raw_temp = ctx
        .acquire_temps(1)
        .inspect_err(|_| ctx.release_temps(argc + 1))?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // 1) Evaluate the tag expression → callee_temp.
        lower_return_expression(builder, ctx, &tagged.tag)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(callee_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (tagged tag): {err:?}"))
            })?;

        // 2) Build the cooked strings array directly into
        //    args_base[0] — it becomes the first argument to the
        //    tag call.
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateArray (tagged cooked): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(args_base))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (tagged cooked arr): {err:?}"))
            })?;
        for quasi in tpl.quasis.iter() {
            // Per §13.2.8.5, invalid escape sequences leave
            // cooked as `undefined`; unsupported for now so we
            // stay clear of the spec's `undefined` entry shape.
            let cooked = quasi.value.cooked.as_deref().ok_or_else(|| {
                SourceLoweringError::unsupported("invalid_template_escape", quasi.span)
            })?;
            let cooked_idx = ctx.intern_string_literal(cooked)?;
            builder
                .emit(Opcode::LdaConstStr, &[Operand::Idx(cooked_idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaConstStr (tagged cooked): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(args_base))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode ArrayPush (tagged cooked): {err:?}"
                    ))
                })?;
        }

        // 3) Build the raw strings array.
        builder.emit(Opcode::CreateArray, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode CreateArray (tagged raw): {err:?}"))
        })?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(raw_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (tagged raw arr): {err:?}"))
            })?;
        for quasi in tpl.quasis.iter() {
            let raw_idx = ctx.intern_string_literal(quasi.value.raw.as_str())?;
            builder
                .emit(Opcode::LdaConstStr, &[Operand::Idx(raw_idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaConstStr (tagged raw): {err:?}"
                    ))
                })?;
            builder
                .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(raw_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode ArrayPush (tagged raw): {err:?}"))
                })?;
        }

        // 4) strings.raw = raw.
        let raw_name_idx = ctx.intern_property_name("raw")?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(raw_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (tagged raw): {err:?}"))
            })?;
        builder
            .emit(
                Opcode::StaNamedProperty,
                &[
                    Operand::Reg(u32::from(args_base)),
                    Operand::Idx(raw_name_idx),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaNamedProperty (tagged raw): {err:?}"
                ))
            })?;

        // 5) Lower each substitution into args_base[1..].
        for (i, expr) in tpl.expressions.iter().enumerate() {
            lower_return_expression(builder, ctx, expr)?;
            let slot = args_base
                .checked_add(RegisterIndex::try_from(i + 1).map_err(|_| {
                    SourceLoweringError::Internal("tagged arg slot overflow".into())
                })?)
                .ok_or_else(|| {
                    SourceLoweringError::Internal("tagged arg slot overflow (add)".into())
                })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (tagged arg): {err:?}"))
                })?;
        }

        // 6) Dispatch with `this = undefined`.
        builder
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
                    "encode CallUndefinedReceiver (tagged): {err:?}"
                ))
            })?;
        Ok(())
    })();

    ctx.release_temps(1); // raw_temp
    ctx.release_temps(argc); // args
    ctx.release_temps(1); // callee_temp
    lower
}

/// Stable tag for unsupported `PropertyKey` shapes — surfaces in
/// `SourceLoweringError::Unsupported { construct }`.
fn property_key_tag(key: &PropertyKey<'_>) -> &'static str {
    match key {
        PropertyKey::StaticIdentifier(_) => "static_identifier_key",
        PropertyKey::PrivateIdentifier(_) => "private_identifier_key",
        PropertyKey::StringLiteral(_) => "string_literal_key",
        PropertyKey::NumericLiteral(_) => "numeric_literal_key",
        PropertyKey::BigIntLiteral(_) => "bigint_literal_key",
        PropertyKey::TemplateLiteral(_) => "template_literal_key",
        // All other expression-inherited variants surface as a
        // generic computed-key tag. Reached only when the AST builds
        // something like `{[expr]: v}` slipping past the `computed`
        // guard — the front wall rejects first.
        _ => "computed_property_key",
    }
}

/// Per-operator opcode pair: the Reg-RHS form and the optional
/// `*Smi imm` fast path. `Some(smi)` means the bytecode ISA carries a
/// dedicated immediate opcode for this operator; `None` means a
/// literal RHS would have to be materialised into a scratch slot.
struct BinaryOpEncoding {
    reg_opcode: Opcode,
    smi_opcode: Option<Opcode>,
    /// `true` when `a OP b == b OP a` (Add/Mul/BitOr/BitAnd/BitXor).
    /// Non-commutative ops (Sub/Shl/Shr/UShr) need a second temp slot
    /// in the complex-RHS fallback to preserve operand order.
    commutative: bool,
    /// Short label used in `SourceLoweringError::Internal` messages so
    /// encoder failures point at the right opcode without resorting to
    /// `format!("{:?}", op)`.
    label: &'static str,
}

/// Maps a parsed binary operator to the v2 opcode pair the lowering
/// uses. Returns `None` for operators outside the M3 int32 surface
/// (comparisons, equality, exponent, division, remainder, membership);
/// callers fall back to [`binary_operator_tag`] for the diagnostic.
fn binary_op_encoding(op: BinaryOperator) -> Option<BinaryOpEncoding> {
    use BinaryOperator::*;
    Some(match op {
        Addition => BinaryOpEncoding {
            reg_opcode: Opcode::Add,
            smi_opcode: Some(Opcode::AddSmi),
            // M15: JS `+` is non-commutative on strings (`"a" + "b"`
            // ≠ `"b" + "a"`) even though int32 addition is. The
            // complex-RHS fallback must preserve LHS/RHS ordering so
            // string concat composes correctly, so the encoding
            // advertises `commutative: false` and takes the 2-temp
            // path. Int32 `a + b` stays correct because it's
            // genuinely commutative; the only cost is one extra temp
            // slot on nested-binary RHS shapes that rarely appear in
            // hot loops.
            commutative: false,
            label: "Add",
        },
        Subtraction => BinaryOpEncoding {
            reg_opcode: Opcode::Sub,
            smi_opcode: Some(Opcode::SubSmi),
            commutative: false,
            label: "Sub",
        },
        Multiplication => BinaryOpEncoding {
            reg_opcode: Opcode::Mul,
            smi_opcode: Some(Opcode::MulSmi),
            commutative: true,
            label: "Mul",
        },
        BitwiseOR => BinaryOpEncoding {
            reg_opcode: Opcode::BitwiseOr,
            smi_opcode: Some(Opcode::BitwiseOrSmi),
            commutative: true,
            label: "BitwiseOr",
        },
        BitwiseAnd => BinaryOpEncoding {
            reg_opcode: Opcode::BitwiseAnd,
            smi_opcode: Some(Opcode::BitwiseAndSmi),
            commutative: true,
            label: "BitwiseAnd",
        },
        BitwiseXOR => BinaryOpEncoding {
            reg_opcode: Opcode::BitwiseXor,
            smi_opcode: None,
            commutative: true,
            label: "BitwiseXor",
        },
        ShiftLeft => BinaryOpEncoding {
            reg_opcode: Opcode::Shl,
            smi_opcode: Some(Opcode::ShlSmi),
            commutative: false,
            label: "Shl",
        },
        ShiftRight => BinaryOpEncoding {
            reg_opcode: Opcode::Shr,
            smi_opcode: Some(Opcode::ShrSmi),
            commutative: false,
            label: "Shr",
        },
        ShiftRightZeroFill => BinaryOpEncoding {
            reg_opcode: Opcode::UShr,
            smi_opcode: None,
            commutative: false,
            label: "UShr",
        },
        Division => BinaryOpEncoding {
            reg_opcode: Opcode::Div,
            smi_opcode: None,
            commutative: false,
            label: "Div",
        },
        Remainder => BinaryOpEncoding {
            reg_opcode: Opcode::Mod,
            smi_opcode: None,
            commutative: false,
            label: "Mod",
        },
        Exponential => BinaryOpEncoding {
            reg_opcode: Opcode::Exp,
            smi_opcode: None,
            commutative: false,
            label: "Exp",
        },
        _ => return None,
    })
}

/// Lowers `lhs <op> rhs` where `<op>` is one of the M3 int32 binary
/// operators and both operands are int32-safe. Picks the `*Smi imm`
/// fast path whenever the RHS is a literal that fits in `i8` and the
/// operator has a dedicated Smi opcode; falls back to the Reg form
/// otherwise. Operators with no Smi opcode (`^`, `>>>`) reject a
/// literal RHS until a future milestone introduces locals to hold it.
fn lower_binary_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &BinaryExpression<'_>,
) -> Result<(), SourceLoweringError> {
    if let Some(encoding) = binary_op_encoding(expr.operator) {
        // LHS must evaluate into the accumulator. Only identifier /
        // int32-safe literal / parenthesised variants of those are
        // allowed — nested binary expressions require a scratch slot
        // we don't allocate yet.
        lower_accumulator_operand(builder, ctx, &expr.left)?;
        return apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right);
    }
    if let Some(rel_encoding) = relational_op_encoding(expr.operator) {
        return lower_relational_expression(builder, ctx, expr, rel_encoding);
    }
    Err(SourceLoweringError::unsupported(
        binary_operator_tag(expr.operator),
        expr.span,
    ))
}

/// Per-operator opcode pair for the M6 relational operators. The
/// dispatcher's `Test*` opcodes all read `acc` as the LHS and a
/// register as the RHS; literal RHS would need a scratch slot which
/// the M6 frame layout does not yet provide. Instead, the lowering
/// **swaps operands** for the `identifier <op> literal` shape — `n <
/// 5` lowers as `LdaSmi 5; TestGreaterThan r_n`, which evaluates
/// `5 > n` and is equivalent to `n < 5`. `swapped_op` carries the
/// inverted-direction opcode for that swap; for symmetric operators
/// (`===`, `!==`) it equals `forward_op`.
struct RelationalOpEncoding {
    forward_op: Opcode,
    swapped_op: Opcode,
    /// `true` for `!==` only — the lowering follows up the
    /// `TestEqualStrict` with a `LogicalNot` so the accumulator
    /// carries the negated boolean.
    requires_inversion: bool,
    label: &'static str,
}

fn relational_op_encoding(op: BinaryOperator) -> Option<RelationalOpEncoding> {
    use BinaryOperator::*;
    Some(match op {
        LessThan => RelationalOpEncoding {
            forward_op: Opcode::TestLessThan,
            swapped_op: Opcode::TestGreaterThan,
            requires_inversion: false,
            label: "TestLessThan",
        },
        GreaterThan => RelationalOpEncoding {
            forward_op: Opcode::TestGreaterThan,
            swapped_op: Opcode::TestLessThan,
            requires_inversion: false,
            label: "TestGreaterThan",
        },
        LessEqualThan => RelationalOpEncoding {
            forward_op: Opcode::TestLessThanOrEqual,
            swapped_op: Opcode::TestGreaterThanOrEqual,
            requires_inversion: false,
            label: "TestLessThanOrEqual",
        },
        GreaterEqualThan => RelationalOpEncoding {
            forward_op: Opcode::TestGreaterThanOrEqual,
            swapped_op: Opcode::TestLessThanOrEqual,
            requires_inversion: false,
            label: "TestGreaterThanOrEqual",
        },
        StrictEquality => RelationalOpEncoding {
            forward_op: Opcode::TestEqualStrict,
            swapped_op: Opcode::TestEqualStrict,
            requires_inversion: false,
            label: "TestEqualStrict",
        },
        StrictInequality => RelationalOpEncoding {
            forward_op: Opcode::TestEqualStrict,
            swapped_op: Opcode::TestEqualStrict,
            requires_inversion: true,
            label: "TestEqualStrict",
        },
        _ => return None,
    })
}

/// Lowers a relational binary expression. The dispatcher's `Test*`
/// opcodes encode `acc <op> reg`, so one operand must reach a
/// register and the other must reach the accumulator. Literal-on-RHS
/// patterns auto-swap to literal-on-LHS form using the `swapped_op`
/// from [`relational_op_encoding`]; two-literal comparisons reject
/// because neither side reaches a register without a scratch slot.
fn lower_relational_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &BinaryExpression<'_>,
    encoding: RelationalOpEncoding,
) -> Result<(), SourceLoweringError> {
    // Direction:
    //   Forward — LHS lowers to acc, RHS is an identifier whose slot
    //              becomes the register operand.
    //   Swap    — RHS literal lowers to acc, LHS identifier becomes
    //              the register operand. Uses `swapped_op` so the
    //              comparison direction is preserved (`n < 5` ≡
    //              `5 > n`).
    enum Direction<'a> {
        Forward {
            rhs_ident: &'a oxc_ast::ast::IdentifierReference<'a>,
        },
        Swap {
            rhs_literal: &'a NumericLiteral<'a>,
            lhs_ident: &'a oxc_ast::ast::IdentifierReference<'a>,
        },
    }

    let direction = match (&expr.left, &expr.right) {
        // identifier OP identifier — Forward
        (Expression::Identifier(_), Expression::Identifier(rhs)) => {
            Direction::Forward { rhs_ident: rhs }
        }
        // literal OP identifier — Forward
        (Expression::NumericLiteral(_), Expression::Identifier(rhs)) => {
            Direction::Forward { rhs_ident: rhs }
        }
        // identifier OP literal — Swap
        (Expression::Identifier(lhs), Expression::NumericLiteral(rhs)) => Direction::Swap {
            rhs_literal: rhs,
            lhs_ident: lhs,
        },
        // Anything else (member access, call, paren, nested
        // binary, literal-literal, two complex sides, …) takes
        // the complex-operand path: lower LHS into a temp, lower
        // RHS into acc, then emit the RHS-form comparison
        // against the temp.
        _ => {
            let lhs_temp = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                lower_return_expression(builder, ctx, &expr.left)?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (relational complex LHS): {err:?}"
                        ))
                    })?;
                lower_return_expression(builder, ctx, &expr.right)?;
                // Acc holds RHS; emit `Test<op>Reg <lhs>` which
                // computes `<lhs> OP <acc>`. Swap direction so
                // the original `lhs OP rhs` meaning holds:
                // `acc < lhs_temp` is `rhs < lhs`, but we want
                // `lhs < rhs`. The `swapped_op` encoding is
                // exactly `lhs OP acc` with lhs as register and
                // rhs in acc — perfect here.
                builder
                    .emit(encoding.swapped_op, &[Operand::Reg(u32::from(lhs_temp))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode {} (relational complex): {err:?}",
                            encoding.label
                        ))
                    })?;
                Ok(())
            })();
            ctx.release_temps(1);
            lower?;
            if encoding.requires_inversion {
                builder.emit(Opcode::LogicalNot, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LogicalNot (relational complex): {err:?}"
                    ))
                })?;
            }
            return Ok(());
        }
    };

    match direction {
        Direction::Forward { rhs_ident } => {
            // RHS register operand requires a user-visible
            // register — upvalues and module globals route
            // through the complex path (LHS spilled, RHS lowered
            // into acc via `LdaUpvalue` / `LdaGlobal`, then
            // `swapped_op` emitted).
            let rhs_binding = ctx.resolve_identifier(rhs_ident.name.as_str());
            let rhs_direct = matches!(
                rhs_binding,
                Some(BindingRef::Param { .. })
                    | Some(BindingRef::Local {
                        initialized: true,
                        ..
                    })
            );
            if !rhs_direct {
                let lhs_temp = ctx.acquire_temps(1)?;
                let lower = (|| -> Result<(), SourceLoweringError> {
                    lower_return_expression(builder, ctx, &expr.left)?;
                    builder
                        .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode Star (relational global LHS): {err:?}"
                            ))
                        })?;
                    lower_return_expression(builder, ctx, &expr.right)?;
                    builder
                        .emit(encoding.swapped_op, &[Operand::Reg(u32::from(lhs_temp))])
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode {} (relational global): {err:?}",
                                encoding.label
                            ))
                        })?;
                    Ok(())
                })();
                ctx.release_temps(1);
                lower?;
                if encoding.requires_inversion {
                    builder.emit(Opcode::LogicalNot, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LogicalNot (relational global): {err:?}"
                        ))
                    })?;
                }
                return Ok(());
            }
            lower_accumulator_operand(builder, ctx, &expr.left)?;
            let binding = rhs_binding.expect("checked Some above");
            emit_identifier_as_reg_operand(
                builder,
                ctx,
                encoding.forward_op,
                encoding.label,
                binding,
                rhs_ident.span,
            )?;
        }
        Direction::Swap {
            rhs_literal,
            lhs_ident,
        } => {
            let value = int32_from_literal(rhs_literal)?;
            builder
                .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaSmi: {err:?}")))?;
            let binding = ctx
                .resolve_identifier(lhs_ident.name.as_str())
                .ok_or_else(|| {
                    SourceLoweringError::unsupported("unbound_identifier", lhs_ident.span)
                })?;
            emit_identifier_as_reg_operand(
                builder,
                ctx,
                encoding.swapped_op,
                encoding.label,
                binding,
                lhs_ident.span,
            )?;
        }
    }

    if encoding.requires_inversion {
        builder
            .emit(Opcode::LogicalNot, &[])
            .map_err(|err| SourceLoweringError::Internal(format!("encode LogicalNot: {err:?}")))?;
    }

    Ok(())
}

/// Emits an opcode that takes an identifier-bound register as its
/// sole operand (e.g. `Add r_n`, `TestLessThan r_n`). Performs the
/// shared TDZ check on the binding so callers don't have to repeat
/// the match. Used by [`lower_identifier_as_reg_rhs`] (arithmetic
/// RHS) and [`lower_relational_expression`] (relational comparand).
///
/// Allocates an arithmetic feedback slot and attaches it to the
/// emitted instruction. Both arithmetic RHS loads and relational
/// RHS loads benefit from the int32-trust elision in the JIT
/// baseline, so the attachment is unconditional — the feedback
/// lattice's monotonic semantics (observe_int32 only ever records
/// Int32 when both operands were int32) preserves correctness across
/// the two call kinds.
fn emit_identifier_as_reg_operand(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    opcode: Opcode,
    label: &'static str,
    binding: BindingRef,
    ident_span: Span,
) -> Result<u32, SourceLoweringError> {
    let direct_reg = match binding {
        BindingRef::Param { reg } => Some(reg),
        BindingRef::Local {
            reg,
            initialized: true,
            runtime_tdz: false,
            ..
        } => Some(reg),
        BindingRef::Local {
            runtime_tdz: true, ..
        } => None,
        BindingRef::Local {
            initialized: false,
            runtime_tdz: false,
            ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident_span,
            ));
        }
        BindingRef::Upvalue { .. } => None,
    };
    if let Some(reg) = direct_reg {
        let pc = builder
            .emit(opcode, &[Operand::Reg(u32::from(reg))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {label}: {err:?}")))?;
        let slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(pc, slot);
        return Ok(pc);
    }

    let lhs_temp = ctx.acquire_temps(1)?;
    let rhs_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let result = (|| -> Result<u32, SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star ({label} lhs temp): {err:?}"))
            })?;
        emit_load_binding_value(builder, binding, ident_span, label)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star ({label} rhs temp): {err:?}"))
            })?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar ({label} lhs reload): {err:?}"))
            })?;
        let pc = builder
            .emit(opcode, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode {label}: {err:?}")))?;
        let slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(pc, slot);
        Ok(pc)
    })();
    ctx.release_temps(2);
    result
}

/// Applies a binary operation whose LHS is already in the accumulator.
/// Picks `*Smi imm` for int32-safe literal RHS that fits `i8` (when
/// the operator carries a Smi opcode), or the Reg form for an
/// in-scope identifier RHS. Falls back to a temp-spill path for
/// "complex" RHS shapes (call, nested binary, parenthesised binary,
/// assignment) — the LHS gets spilled to a temp, the RHS is lowered
/// into acc through the standard expression path, and the result is
/// stitched back together as `acc = LHS op RHS` (commutative ops
/// reuse one temp; non-commutative ops grab a second temp to
/// preserve operand order).
///
/// Used by both [`lower_binary_expression`] and the compound-
/// assignment path in [`lower_assignment_expression`] — the
/// bytecode shape `<load lhs into acc>; <op> <rhs>` is identical.
fn apply_binary_op_with_acc_lhs(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    encoding: &BinaryOpEncoding,
    rhs: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    match rhs {
        Expression::NumericLiteral(literal) => {
            // If the operator has a dedicated `*Smi` opcode AND
            // the literal fits `i8`, take the fast path. Otherwise
            // — no Smi opcode (`^`, `>>>`, `/`, `%`, `**`), wide
            // literal, or fractional literal — spill to the
            // generic RHS path so the value goes through a temp
            // register and the Reg-form opcode does the work.
            let fits_i8 = int32_from_literal(literal)
                .ok()
                .map(|v| (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&v));
            if let (Some(smi_op), Some(true)) = (encoding.smi_opcode, fits_i8) {
                let value = int32_from_literal(literal)?;
                let pc = builder
                    .emit(smi_op, &[Operand::Imm(value)])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode {}Smi: {err:?}",
                            encoding.label
                        ))
                    })?;
                let slot = ctx.allocate_arithmetic_feedback();
                builder.attach_feedback(pc, slot);
                return Ok(());
            }
            apply_binary_op_with_complex_rhs(builder, ctx, encoding, rhs)
        }
        Expression::Identifier(ident) => {
            // §M35 module globals (imports, top-level exports) and
            // upvalue bindings don't live in a user-visible
            // register — both route through the complex-RHS spill
            // path so the RHS is read via `LdaGlobal` /
            // `LdaUpvalue` into acc and stitched against the
            // spilled LHS. Only params / initialised locals can
            // feed the fast `Op reg` shape.
            match ctx.resolve_identifier(ident.name.as_str()) {
                Some(binding) if !matches!(binding, BindingRef::Upvalue { .. }) => {
                    lower_identifier_as_reg_rhs(builder, ctx, encoding, binding, ident.span)
                }
                _ => apply_binary_op_with_complex_rhs(builder, ctx, encoding, rhs),
            }
        }
        // Complex RHS shapes — a call, a nested binary, a
        // parenthesised binary, an assignment expression, a unary /
        // update expression, a null/boolean/string literal, etc.
        // The RHS lowering would clobber acc (which currently holds
        // the LHS), so we spill LHS to a temp first, then re-stitch.
        Expression::CallExpression(_)
        | Expression::BinaryExpression(_)
        | Expression::ParenthesizedExpression(_)
        | Expression::AssignmentExpression(_)
        | Expression::UnaryExpression(_)
        | Expression::UpdateExpression(_)
        | Expression::ConditionalExpression(_)
        | Expression::LogicalExpression(_)
        | Expression::StringLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::ObjectExpression(_)
        | Expression::ArrayExpression(_)
        | Expression::StaticMemberExpression(_)
        | Expression::ComputedMemberExpression(_)
        | Expression::TemplateLiteral(_) => {
            apply_binary_op_with_complex_rhs(builder, ctx, encoding, rhs)
        }
        other => Err(SourceLoweringError::unsupported(
            expression_construct_tag(other),
            other.span(),
        )),
    }
}

/// Fallback path for binary expressions whose RHS doesn't fit the
/// fast `*Smi imm` / `Op reg` shapes — typically because the RHS
/// itself contains a call, a nested binary, or an assignment.
///
/// Bytecode shape (commutative op, single temp):
///
/// ```text
///   ; LHS already in acc (lowered by caller)
///   Star r_lhs_temp      ; spill LHS so RHS can clobber acc
///   <lower RHS>          ; acc = RHS
///   Op r_lhs_temp        ; acc = RHS op LHS = LHS op RHS  (commutative)
/// ```
///
/// For non-commutative ops we need a second temp to preserve
/// operand order:
///
/// ```text
///   Star r_lhs_temp
///   <lower RHS>
///   Star r_rhs_temp
///   Ldar r_lhs_temp      ; acc = LHS
///   Op r_rhs_temp        ; acc = LHS op RHS
/// ```
fn apply_binary_op_with_complex_rhs(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    encoding: &BinaryOpEncoding,
    rhs: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    let lhs_temp = ctx.acquire_temps(1)?;
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(lhs_temp))])
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Star (LHS spill): {err:?}"))
        })?;

    let lower_result = lower_return_expression(builder, ctx, rhs);
    if let Err(err) = lower_result {
        ctx.release_temps(1);
        return Err(err);
    }

    if encoding.commutative {
        // acc = RHS, lhs_temp = LHS. `Op r_lhs_temp` ⇒ acc = RHS
        // op LHS, which equals LHS op RHS for commutative ops.
        let pc = builder
            .emit(encoding.reg_opcode, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode {} (commutative complex RHS): {err:?}",
                    encoding.label
                ))
            })?;
        let slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(pc, slot);
        ctx.release_temps(1);
        Ok(())
    } else {
        // Non-commutative: order matters. Spill RHS to a second
        // temp, reload LHS into acc, then apply op against RHS.
        let rhs_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (RHS spill): {err:?}"))
            })?;
        let ldar_pc = builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(lhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (LHS reload): {err:?}"))
            })?;
        let ldar_slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(ldar_pc, ldar_slot);
        let pc = builder
            .emit(encoding.reg_opcode, &[Operand::Reg(u32::from(rhs_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode {} (non-commutative complex RHS): {err:?}",
                    encoding.label
                ))
            })?;
        let slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(pc, slot);
        // Release in LIFO order — rhs_temp was acquired last.
        ctx.release_temps(1); // rhs_temp
        ctx.release_temps(1); // lhs_temp
        Ok(())
    }
}

/// Lowers `target <op>= rhs` (or `target = rhs`) onto a local `let`
/// slot. Leaves the assigned value in the accumulator so nested
/// assignments (`let y = x = 5;`, `return x = 5;`) compose without
/// extra Ldar / Star round-trips.
///
/// Bytecode shape:
/// - `x = rhs` →  `<lower rhs>; Star r_x`
/// - `x += rhs` → `Ldar r_x; <Add/AddSmi rhs>; Star r_x`
/// - other compound forms identical, with the matching binary opcode.
///
/// Rejects:
/// - non-identifier target (member, destructuring, TS-only) →
///   stable per-shape tag;
/// - unbound identifier → `unbound_identifier`;
/// - const binding as target → `const_assignment`;
/// - in-TDZ binding as target → `tdz_self_reference`;
/// - assignment operator outside `=`/`+=`/`-=`/`*=`/`|=` → stable
///   per-operator tag (e.g. `division_assign`).
fn lower_assignment_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &AssignmentExpression<'_>,
) -> Result<(), SourceLoweringError> {
    // Dispatch on target shape. Identifier + static/computed member
    // are the three supported write targets as of M17. Everything
    // else (private fields, destructuring, TS-only) stays rejected
    // with a stable per-shape tag so future widenings don't have to
    // unify the error-surface story retroactively.
    match &expr.left {
        AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            lower_identifier_assignment(builder, ctx, expr, ident)
        }
        AssignmentTarget::StaticMemberExpression(member) => {
            lower_static_member_assignment(builder, ctx, expr, member)
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            lower_computed_member_assignment(builder, ctx, expr, member)
        }
        AssignmentTarget::PrivateFieldExpression(member) => {
            lower_private_field_assignment(builder, ctx, expr, member)
        }
        AssignmentTarget::ArrayAssignmentTarget(pattern) => {
            lower_array_destructuring_assignment(builder, ctx, expr, pattern)
        }
        AssignmentTarget::ObjectAssignmentTarget(pattern) => {
            lower_object_destructuring_assignment(builder, ctx, expr, pattern)
        }
        // TS-only assignment targets (`x as T = ...`, `x! = ...`,
        // etc.). Treated as one bucket — all are out of scope until
        // the source compiler grows TS-specific handling.
        AssignmentTarget::TSAsExpression(_)
        | AssignmentTarget::TSSatisfiesExpression(_)
        | AssignmentTarget::TSNonNullExpression(_)
        | AssignmentTarget::TSTypeAssertion(_) => Err(SourceLoweringError::unsupported(
            "ts_assignment_target",
            expr.span,
        )),
    }
}

/// Identifier-target path for `lower_assignment_expression`. Preserves
/// the original M5 semantics: local `let` only, rejects `const`, TDZ,
/// and param writes; compound `<op>=` emits `Ldar r_x; <apply op>;
/// Star r_x`.
/// Destructuring assignment to an array-shaped target:
/// `[a, b, c] = arr` (no `let` keyword — assigns to EXISTING
/// bindings). Evaluates the RHS once into a temp, then for each
/// element emits a `LdaKeyedProperty` read + assign to the
/// element target. Supports defaults, nested patterns, and rest.
/// Leaves the RHS value in the accumulator so the assignment
/// expression yields the source object per §13.15.
fn lower_array_destructuring_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a AssignmentExpression<'a>,
    pattern: &'a oxc_ast::ast::ArrayAssignmentTarget<'a>,
) -> Result<(), SourceLoweringError> {
    if !matches!(expr.operator, AssignmentOperator::Assign) {
        return Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            pattern.span,
        ));
    }
    let src_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        // RHS → temp.
        lower_return_expression(builder, ctx, &expr.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (array destruct src): {err:?}"))
            })?;
        destructure_array_assignment_from_temp(builder, ctx, pattern, src_temp)?;
        // Leave the RHS value in acc so the assignment-expression
        // yields the source per §13.15.2.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Ldar (array destruct yield): {err:?}"
                ))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Destructuring assignment to an object-shaped target:
/// `({ a, b: c, ...rest } = obj)`.
fn lower_object_destructuring_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a AssignmentExpression<'a>,
    pattern: &'a oxc_ast::ast::ObjectAssignmentTarget<'a>,
) -> Result<(), SourceLoweringError> {
    if !matches!(expr.operator, AssignmentOperator::Assign) {
        return Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            pattern.span,
        ));
    }
    let src_temp = ctx.acquire_temps(1)?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        lower_return_expression(builder, ctx, &expr.right)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (obj destruct src): {err:?}"))
            })?;
        destructure_object_assignment_from_temp(builder, ctx, pattern, src_temp)?;
        // Yield the RHS as the assignment's value.
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(src_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (obj destruct yield): {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1);
    lower
}

/// Assigns the accumulator (already holding the right value) to
/// a destructuring-assignment leaf. Handles the `MaybeDefault`
/// wrapper by running the default-check first.
fn assign_destructured_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    target: &'a oxc_ast::ast::AssignmentTargetMaybeDefault<'a>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::AssignmentTargetMaybeDefault as M;
    match target {
        M::AssignmentTargetWithDefault(wd) => {
            emit_default_for_destructured_leaf(builder, ctx, Some(&wd.init))?;
            assign_destructured_target_from_assignment_target(builder, ctx, &wd.binding)
        }
        // The `inherit_variants!` macro ensures every
        // `AssignmentTarget` variant is mirrored as a
        // `MaybeDefault` variant with the same discriminant
        // range — match each explicitly so the compiler's
        // exhaustiveness check stays honest.
        M::AssignmentTargetIdentifier(ident) => assign_identifier_reference(builder, ctx, ident),
        M::StaticMemberExpression(member) => assign_static_member(builder, ctx, member),
        M::ComputedMemberExpression(member) => assign_computed_member(builder, ctx, member),
        M::ArrayAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested array destruct): {err:?}"
                        ))
                    })?;
                destructure_array_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        M::ObjectAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested obj destruct): {err:?}"
                        ))
                    })?;
                destructure_object_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        _ => Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            target.span(),
        )),
    }
}

fn assign_static_member<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    member: &'a StaticMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let val_temp = ctx.acquire_temps(1)?;
    let recv_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct static member val): {err:?}"
                ))
            })?;
        lower_return_expression(builder, ctx, &member.object)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(recv_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct static member recv): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Ldar (destruct static member reload): {err:?}"
                ))
            })?;
        let prop_idx = ctx.intern_property_name(member.property.name.as_str())?;
        builder
            .emit(
                Opcode::StaNamedProperty,
                &[Operand::Reg(u32::from(recv_temp)), Operand::Idx(prop_idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaNamedProperty (destruct static member): {err:?}"
                ))
            })?;
        Ok(())
    })();
    ctx.release_temps(2);
    lower
}

fn assign_computed_member<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    member: &'a ComputedMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    let val_temp = ctx.acquire_temps(1)?;
    let recv_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
    let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(2))?;
    let lower = (|| -> Result<(), SourceLoweringError> {
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct computed val): {err:?}"
                ))
            })?;
        lower_return_expression(builder, ctx, &member.object)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(recv_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct computed recv): {err:?}"
                ))
            })?;
        lower_return_expression(builder, ctx, &member.expression)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Star (destruct computed key): {err:?}"
                ))
            })?;
        builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(val_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode Ldar (destruct computed reload): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::StaKeyedProperty,
                &[
                    Operand::Reg(u32::from(recv_temp)),
                    Operand::Reg(u32::from(key_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode StaKeyedProperty (destruct computed): {err:?}"
                ))
            })?;
        Ok(())
    })();
    ctx.release_temps(3);
    lower
}

/// Routes an already-loaded accumulator value to an
/// `AssignmentTarget`. Used by destructuring-assignment elements
/// + rest targets.
fn assign_destructured_target_from_assignment_target<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    target: &'a AssignmentTarget<'a>,
) -> Result<(), SourceLoweringError> {
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            assign_identifier_reference(builder, ctx, ident)
        }
        AssignmentTarget::StaticMemberExpression(member) => {
            assign_static_member(builder, ctx, member)
        }
        AssignmentTarget::ComputedMemberExpression(member) => {
            assign_computed_member(builder, ctx, member)
        }
        AssignmentTarget::ArrayAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested array destruct): {err:?}"
                        ))
                    })?;
                destructure_array_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        AssignmentTarget::ObjectAssignmentTarget(nested) => {
            let nested_src = ctx.acquire_temps(1)?;
            let lower = (|| -> Result<(), SourceLoweringError> {
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(nested_src))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode Star (nested obj destruct): {err:?}"
                        ))
                    })?;
                destructure_object_assignment_from_temp(builder, ctx, nested, nested_src)
            })();
            ctx.release_temps(1);
            lower
        }
        other => Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            other.span(),
        )),
    }
}

fn destructure_array_assignment_from_temp<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    pattern: &'a oxc_ast::ast::ArrayAssignmentTarget<'a>,
    src_temp: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    for (index, element) in pattern.elements.iter().enumerate() {
        let Some(elem) = element.as_ref() else {
            continue;
        };
        let idx_i32 = i32::try_from(index).map_err(|_| {
            SourceLoweringError::Internal("nested array destruct assign index overflow".into())
        })?;
        builder
            .emit(Opcode::LdaSmi, &[Operand::Imm(idx_i32)])
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaSmi (nested array destruct): {err:?}"
                ))
            })?;
        builder
            .emit(
                Opcode::LdaKeyedProperty,
                &[Operand::Reg(u32::from(src_temp))],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!(
                    "encode LdaKeyedProperty (nested array destruct): {err:?}"
                ))
            })?;
        assign_destructured_target(builder, ctx, elem)?;
    }
    if let Some(rest) = pattern.rest.as_deref() {
        let slice_target = ctx.acquire_temps(1)?;
        let slice_lower = (|| -> Result<(), SourceLoweringError> {
            emit_array_rest_slice(builder, ctx, src_temp, pattern.elements.len(), slice_target)?;
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(slice_target))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (array destruct rest): {err:?}"
                    ))
                })?;
            assign_destructured_target_from_assignment_target(builder, ctx, &rest.target)?;
            Ok(())
        })();
        ctx.release_temps(1);
        slice_lower?;
    }
    Ok(())
}

fn destructure_object_assignment_from_temp<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    pattern: &'a oxc_ast::ast::ObjectAssignmentTarget<'a>,
    src_temp: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    let mut excluded_keys: Vec<String> = Vec::new();
    for prop in pattern.properties.iter() {
        match prop {
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                let name = p.binding.name.as_str().to_owned();
                excluded_keys.push(name.clone());
                let key_idx = ctx.intern_property_name(&name)?;
                builder
                    .emit(
                        Opcode::LdaNamedProperty,
                        &[Operand::Reg(u32::from(src_temp)), Operand::Idx(key_idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaNamedProperty (nested obj destruct): {err:?}"
                        ))
                    })?;
                if let Some(default_expr) = &p.init {
                    emit_default_for_destructured_leaf(builder, ctx, Some(default_expr))?;
                }
                assign_identifier_reference(builder, ctx, &p.binding)?;
            }
            oxc_ast::ast::AssignmentTargetProperty::AssignmentTargetPropertyProperty(kv) => {
                let (key_idx, key_is_computed, key_name_for_rest) = match &kv.name {
                    PropertyKey::StaticIdentifier(ident) => {
                        let name = ident.name.as_str().to_owned();
                        let idx = ctx.intern_property_name(&name)?;
                        (Some(idx), false, Some(name))
                    }
                    PropertyKey::StringLiteral(lit) => {
                        let name = lit.value.as_str().to_owned();
                        let idx = ctx.intern_property_name(&name)?;
                        (Some(idx), false, Some(name))
                    }
                    other => {
                        let key_temp = ctx.acquire_temps(1)?;
                        let result = (|| -> Result<(), SourceLoweringError> {
                            lower_return_expression(builder, ctx, other.to_expression())?;
                            builder
                                .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode Star (obj destruct key): {err:?}"
                                    ))
                                })?;
                            builder
                                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(key_temp))])
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode Ldar (obj destruct key): {err:?}"
                                    ))
                                })?;
                            builder
                                .emit(
                                    Opcode::LdaKeyedProperty,
                                    &[Operand::Reg(u32::from(src_temp))],
                                )
                                .map_err(|err| {
                                    SourceLoweringError::Internal(format!(
                                        "encode LdaKeyedProperty (obj destruct): {err:?}"
                                    ))
                                })?;
                            Ok(())
                        })();
                        ctx.release_temps(1);
                        result?;
                        (None, true, None)
                    }
                };
                if !key_is_computed && let Some(idx) = key_idx {
                    builder
                        .emit(
                            Opcode::LdaNamedProperty,
                            &[Operand::Reg(u32::from(src_temp)), Operand::Idx(idx)],
                        )
                        .map_err(|err| {
                            SourceLoweringError::Internal(format!(
                                "encode LdaNamedProperty (nested obj destruct kv): {err:?}"
                            ))
                        })?;
                }
                if let Some(name) = key_name_for_rest {
                    excluded_keys.push(name);
                }
                assign_destructured_target(builder, ctx, &kv.binding)?;
            }
        }
    }
    if let Some(rest) = pattern.rest.as_deref() {
        let rest_target = ctx.acquire_temps(1)?;
        let rest_lower = (|| -> Result<(), SourceLoweringError> {
            emit_object_rest_copy(builder, ctx, src_temp, &excluded_keys, rest_target)?;
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(rest_target))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (obj destruct rest): {err:?}"
                    ))
                })?;
            assign_destructured_target_from_assignment_target(builder, ctx, &rest.target)?;
            Ok(())
        })();
        ctx.release_temps(1);
        rest_lower?;
    }
    Ok(())
}

/// Assigns acc to an existing identifier reference —
/// `lower_identifier_assignment`'s core work without the
/// compound-operator logic. Used by destructuring assignment.
fn assign_identifier_reference<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    ident: &'a IdentifierReference<'a>,
) -> Result<(), SourceLoweringError> {
    let name = ident.name.as_str();
    let Some(binding) = ctx.resolve_identifier(name) else {
        return Err(SourceLoweringError::unsupported(
            "unbound_identifier",
            ident.span,
        ));
    };
    match binding {
        BindingRef::Param { reg }
        | BindingRef::Local {
            reg,
            initialized: true,
            is_const: false,
            runtime_tdz: false,
            ..
        } => {
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (destruct ident target): {err:?}"
                    ))
                })?;
            Ok(())
        }
        BindingRef::Local {
            reg,
            is_const: false,
            runtime_tdz: true,
            ..
        } => {
            emit_assert_binding_ready_for_write(
                builder,
                binding,
                ident.span,
                "destruct ident target",
            )?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (destruct ident target): {err:?}"
                    ))
                })?;
            Ok(())
        }
        BindingRef::Local { is_const: true, .. } => Err(SourceLoweringError::unsupported(
            "const_assignment",
            ident.span,
        )),
        BindingRef::Local {
            initialized: false, ..
        } => Err(SourceLoweringError::unsupported(
            "tdz_self_reference",
            ident.span,
        )),
        BindingRef::Upvalue { idx } => {
            emit_assert_binding_ready_for_write(
                builder,
                binding,
                ident.span,
                "destruct ident target",
            )?;
            builder
                .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(idx))])
                .map(|_| ())
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode StaUpvalue (destruct ident target): {err:?}"
                    ))
                })
        }
    }
}

fn lower_identifier_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &AssignmentExpression<'a>,
    ident: &IdentifierReference<'a>,
) -> Result<(), SourceLoweringError> {
    let target_ident = ident.name.as_str();
    let target_span = ident.span;
    let binding = ctx
        .resolve_identifier(target_ident)
        .ok_or_else(|| SourceLoweringError::unsupported("unbound_identifier", target_span))?;

    // M25: assignment to an upvalue target goes through
    // `StaUpvalue` — a different shape from the register-based
    // path, so handle it separately.
    if let BindingRef::Upvalue { idx } = binding {
        if expr.operator == AssignmentOperator::Assign {
            emit_assert_binding_ready_for_write(builder, binding, target_span, "assign upvalue")?;
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            emit_load_binding_value(builder, binding, target_span, "compound upvalue lhs")?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        builder
            .emit(Opcode::StaUpvalue, &[Operand::Idx(u32::from(idx))])
            .map_err(|err| SourceLoweringError::Internal(format!("encode StaUpvalue: {err:?}")))?;
        return Ok(());
    }

    let target_reg = match binding {
        BindingRef::Local {
            reg,
            initialized: true,
            is_const: false,
            runtime_tdz: false,
            ..
        } => reg,
        BindingRef::Local {
            reg,
            runtime_tdz: true,
            ..
        } => {
            emit_assert_binding_ready_for_write(builder, binding, target_span, "assignment lhs")?;
            reg
        }
        BindingRef::Local { is_const: true, .. } => {
            return Err(SourceLoweringError::unsupported(
                "const_assignment",
                target_span,
            ));
        }
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                target_span,
            ));
        }
        // Parameters are ordinary writable bindings in
        // non-strict mode (§10.2.1 FunctionDeclarationInstantiation
        // puts them on the function's VariableEnvironment with
        // `mutable: true`). Assignment writes back into the
        // parameter slot.
        BindingRef::Param { reg } => reg,
        BindingRef::Upvalue { .. } => unreachable!("handled above"),
    };

    if expr.operator == AssignmentOperator::Assign {
        lower_return_expression(builder, ctx, &expr.right)?;
    } else {
        let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
            SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
        })?;
        let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
            SourceLoweringError::Internal(format!(
                "compound assignment {bin_op:?} has no binary opcode encoding"
            ))
        })?;
        if matches!(
            binding,
            BindingRef::Local {
                initialized: true,
                ..
            }
        ) {
            let ldar_pc = builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(target_reg))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Ldar (compound lhs): {err:?}"))
                })?;
            let ldar_slot = ctx.allocate_arithmetic_feedback();
            builder.attach_feedback(ldar_pc, ldar_slot);
        } else {
            emit_load_binding_value(builder, binding, target_span, "compound lhs")?;
        }
        apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
    }

    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(target_reg))])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Star: {err:?}")))?;
    Ok(())
}

/// Lowers `o.x = v` (or `o.x <op>= v`). Shape for plain `=`:
///
/// ```text
///   <materialize base into r_base>
///   <lower v into acc>
///   StaNamedProperty r_base, name_idx
/// ```
///
/// Compound `<op>=` (`+=`, `-=`, `*=`, `|=`):
///
/// ```text
///   <materialize base into r_base>
///   LdaNamedProperty r_base, name_idx   ; acc = o.x
///   <apply_binary_op_with_acc_lhs>       ; acc = o.x <op> v
///   StaNamedProperty r_base, name_idx    ; o.x = acc
/// ```
///
/// The accumulator holds the assigned value on exit, so composed
/// forms (`let y = o.x = 5;`) work without extra traffic.
fn lower_static_member_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &AssignmentExpression<'a>,
    member: &StaticMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            member.span,
        ));
    }
    // M28: `super.x = v` / `super.x <op>= v`. The super base is not
    // materialised into a regular register; instead the LHS read
    // goes through `GetSuperProperty` and the store through
    // `SetSuperProperty`. Receiver register holds the current
    // frame's `this`.
    if matches!(&member.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &member.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let idx = ctx.intern_property_name(member.property.name.as_str())?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super.x write): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super.x receiver): {err:?}"
                    ))
                })?;
            if expr.operator == AssignmentOperator::Assign {
                lower_return_expression(builder, ctx, &expr.right)?;
            } else {
                let bin_op =
                    compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                        SourceLoweringError::unsupported(
                            assignment_operator_tag(expr.operator),
                            expr.span,
                        )
                    })?;
                let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                    SourceLoweringError::Internal(format!(
                        "compound assignment {bin_op:?} has no binary opcode encoding"
                    ))
                })?;
                builder
                    .emit(
                        Opcode::GetSuperProperty,
                        &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                    )
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode GetSuperProperty (compound lhs): {err:?}"
                        ))
                    })?;
                apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
            }
            builder
                .emit(
                    Opcode::SetSuperProperty,
                    &[Operand::Reg(u32::from(receiver_temp)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode SetSuperProperty: {err:?}"))
                })?;
            Ok(())
        })();
        ctx.release_temps(1);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &member.object)?;
    let idx = ctx.intern_property_name(member.property.name.as_str())?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        if expr.operator == AssignmentOperator::Assign {
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            builder
                .emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaNamedProperty (compound): {err:?}"
                    ))
                })?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        builder
            .emit(
                Opcode::StaNamedProperty,
                &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode StaNamedProperty: {err:?}"))
            })?;
        Ok(())
    })();
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

/// Lowers `o[k] = v` (or `o[k] <op>= v`). Shape for plain `=`:
///
/// ```text
///   <materialize base into r_base>
///   <lower key into acc>; Star r_key
///   <lower v into acc>
///   StaKeyedProperty r_base, r_key
/// ```
///
/// Compound `<op>=`:
///
/// ```text
///   <materialize base into r_base>
///   <lower key into acc>; Star r_key
///   Ldar r_key                       ; acc = key
///   LdaKeyedProperty r_base          ; acc = r_base[key]
///   <apply_binary_op_with_acc_lhs>   ; acc = old <op> v
///   StaKeyedProperty r_base, r_key
/// ```
///
/// The key always spills into a dedicated temp so both the read
/// path (which needs key in acc) and the store path (which needs
/// key in a register via `StaKeyedProperty`'s second operand) can
/// reach it.
fn lower_computed_member_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &AssignmentExpression<'a>,
    member: &ComputedMemberExpression<'a>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            member.span,
        ));
    }
    // M28: `super[k] = v` / `super[k] <op>= v`. Receiver is `this`;
    // key is spilled to a dedicated temp; writes go through
    // `SetSuperPropertyComputed`.
    if matches!(&member.object, Expression::Super(_)) {
        enforce_super_property_binding(ctx, &member.object)?;
        let receiver_temp = ctx.acquire_temps(1)?;
        let key_temp = ctx.acquire_temps(1).inspect_err(|_| ctx.release_temps(1))?;
        let lower = (|| -> Result<(), SourceLoweringError> {
            builder.emit(Opcode::LdaThis, &[]).map_err(|err| {
                SourceLoweringError::Internal(format!("encode LdaThis (super[k] write): {err:?}"))
            })?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(receiver_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Star (super[k] receiver): {err:?}"
                    ))
                })?;
            lower_return_expression(builder, ctx, &member.expression)?;
            builder
                .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode Star (super[k] key): {err:?}"))
                })?;
            if expr.operator == AssignmentOperator::Assign {
                lower_return_expression(builder, ctx, &expr.right)?;
            } else {
                let bin_op =
                    compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                        SourceLoweringError::unsupported(
                            assignment_operator_tag(expr.operator),
                            expr.span,
                        )
                    })?;
                let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                    SourceLoweringError::Internal(format!(
                        "compound assignment {bin_op:?} has no binary opcode encoding"
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
                            "encode GetSuperPropertyComputed (compound lhs): {err:?}"
                        ))
                    })?;
                apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
            }
            builder
                .emit(
                    Opcode::SetSuperPropertyComputed,
                    &[
                        Operand::Reg(u32::from(receiver_temp)),
                        Operand::Reg(u32::from(key_temp)),
                    ],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode SetSuperPropertyComputed: {err:?}"
                    ))
                })?;
            Ok(())
        })();
        ctx.release_temps(2);
        return lower;
    }
    let base = materialize_member_base(builder, ctx, &member.object)?;
    let key_temp = ctx.acquire_temps(1)?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        // Evaluate the key into its own temp — JS spec §13.15.2
        // specifies left-to-right evaluation for `o[k] = v`.
        lower_return_expression(builder, ctx, &member.expression)?;
        builder
            .emit(Opcode::Star, &[Operand::Reg(u32::from(key_temp))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Star (computed key spill): {err:?}"))
            })?;

        if expr.operator == AssignmentOperator::Assign {
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            // Reload key into acc for LdaKeyedProperty.
            builder
                .emit(Opcode::Ldar, &[Operand::Reg(u32::from(key_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode Ldar (computed compound key): {err:?}"
                    ))
                })?;
            builder
                .emit(
                    Opcode::LdaKeyedProperty,
                    &[Operand::Reg(u32::from(base.reg))],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaKeyedProperty (compound): {err:?}"
                    ))
                })?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        builder
            .emit(
                Opcode::StaKeyedProperty,
                &[
                    Operand::Reg(u32::from(base.reg)),
                    Operand::Reg(u32::from(key_temp)),
                ],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode StaKeyedProperty: {err:?}"))
            })?;
        Ok(())
    })();
    ctx.release_temps(1); // key_temp
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

/// M29: lowers `obj.#name = v` / `obj.#name <op>= v` onto
/// `SetPrivateField`. Accumulator holds the value on exit (JS
/// assignment value is the RHS), so compound assignments compose
/// cleanly via `apply_binary_op_with_acc_lhs`.
fn lower_private_field_assignment<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a AssignmentExpression<'a>,
    member: &'a oxc_ast::ast::PrivateFieldExpression<'a>,
) -> Result<(), SourceLoweringError> {
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            member.span,
        ));
    }
    let name = member.field.name.as_str();
    enforce_private_name_declared(ctx, name, member.span)?;
    let base = materialize_member_base(builder, ctx, &member.object)?;
    let idx = ctx.intern_property_name(name)?;

    let lower = (|| -> Result<(), SourceLoweringError> {
        if expr.operator == AssignmentOperator::Assign {
            lower_return_expression(builder, ctx, &expr.right)?;
        } else {
            let bin_op = compound_assign_to_binary_operator(expr.operator).ok_or_else(|| {
                SourceLoweringError::unsupported(assignment_operator_tag(expr.operator), expr.span)
            })?;
            let encoding = binary_op_encoding(bin_op).ok_or_else(|| {
                SourceLoweringError::Internal(format!(
                    "compound assignment {bin_op:?} has no binary opcode encoding"
                ))
            })?;
            builder
                .emit(
                    Opcode::GetPrivateField,
                    &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
                )
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode GetPrivateField (compound): {err:?}"
                    ))
                })?;
            apply_binary_op_with_acc_lhs(builder, ctx, &encoding, &expr.right)?;
        }
        builder
            .emit(
                Opcode::SetPrivateField,
                &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
            )
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode SetPrivateField: {err:?}"))
            })?;
        Ok(())
    })();
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
    lower
}

/// Maps a compound assignment operator to the binary operator whose
/// encoding it should use. Returns `None` only for `=` (handled
/// separately — no underlying binary op) and for the short-circuit
/// logical compounds (`||=`, `&&=`, `??=`) which need guard-
/// evaluation semantics the regular binary lowering doesn't
/// provide.
fn compound_assign_to_binary_operator(op: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    use BinaryOperator as B;
    Some(match op {
        A::Addition => B::Addition,
        A::Subtraction => B::Subtraction,
        A::Multiplication => B::Multiplication,
        A::Division => B::Division,
        A::Remainder => B::Remainder,
        A::Exponential => B::Exponential,
        A::ShiftLeft => B::ShiftLeft,
        A::ShiftRight => B::ShiftRight,
        A::ShiftRightZeroFill => B::ShiftRightZeroFill,
        A::BitwiseOR => B::BitwiseOR,
        A::BitwiseXOR => B::BitwiseXOR,
        A::BitwiseAnd => B::BitwiseAnd,
        _ => return None,
    })
}

/// Stable diagnostic tag for an assignment operator outside the M5
/// supported set. Mirrors [`binary_operator_tag`] in style so callers
/// don't have to round-trip through `Debug`.
fn assignment_operator_tag(op: AssignmentOperator) -> &'static str {
    use AssignmentOperator::*;
    match op {
        Assign => "assign",
        Addition => "addition_assign",
        Subtraction => "subtraction_assign",
        Multiplication => "multiplication_assign",
        Division => "division_assign",
        Remainder => "remainder_assign",
        Exponential => "exponential_assign",
        ShiftLeft => "shift_left_assign",
        ShiftRight => "shift_right_assign",
        ShiftRightZeroFill => "unsigned_shift_right_assign",
        BitwiseOR => "bitwise_or_assign",
        BitwiseXOR => "bitwise_xor_assign",
        BitwiseAnd => "bitwise_and_assign",
        LogicalOr => "logical_or_assign",
        LogicalAnd => "logical_and_assign",
        LogicalNullish => "logical_nullish_assign",
    }
}

/// Lowers an expression into the accumulator. This is the same
/// surface as [`lower_return_expression`] — the helper exists as an
/// alias kept for the binary/relational-LHS call sites so future
/// readers see "the LHS lowers via the standard expression path"
/// rather than chasing through `lower_return_expression`.
///
/// Accepting binary and assignment expressions on the LHS unlocks
/// the bench2 idiom `(s + i) | 0`: the parenthesised binary lowers
/// into acc cleanly (binary operations always produce their result
/// in acc), and the outer `| 0` then operates against that acc.
fn lower_accumulator_operand(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    lower_return_expression(builder, ctx, expr)
}

/// Lowers a `CallExpression`. Three callee shapes are accepted:
///
/// - Identifier naming a top-level `FunctionDeclaration` — emits
///   `CallDirect func_idx, argv` for the tightest invocation path
///   (known callee, direct index, tier-up-friendly).
/// - `o.method(args)` (StaticMemberExpression callee) — emits
///   `CallProperty r_callee, r_receiver, argv`; `this` is bound to
///   the member's base per §13.3.6.
/// - `o[k](args)` (ComputedMemberExpression callee) — same opcode,
///   key resolved via `LdaKeyedProperty`.
///
/// Everything else (parenthesised non-identifier, CallExpression
/// callee, …) still rejects with `non_identifier_callee` — those
/// require first-class function values that land in later
/// milestones.
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
fn lower_call_expression(
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
    // `CallSpread` expects a receiver (direct calls don't have
    // one), so direct-call-with-spread is rejected until a future
    // milestone exposes top-level function handles as values.
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
        other => Err(SourceLoweringError::unsupported(
            "non_identifier_callee",
            other.span(),
        )),
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
            builder
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
        builder
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
            builder
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
            builder
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
            Ok(())
        })();
        ctx.release_temps(argc);
        ctx.release_temps(1);
        return lower;
    }
    // Last resort: the name isn't a top-level function or a
    // local / param / upvalue, but may still be a whitelisted
    // global (e.g. `queueMicrotask(cb)` / `setTimeout(cb, 0)`)
    // or — M35 — an `import`-ed binding / top-level `export`ed
    // declaration installed on the global object by
    // `populate_import_globals` / the synthesised module-init.
    // Route through the same `LdaGlobal` path the identifier
    // reference uses, then dispatch as a plain closure call.
    // The call-site whitelist mirrors the identifier-reference
    // whitelist in `lower_identifier_reference`; when either
    // expands, both do. This keeps `globalFn(args)` and `let g =
    // globalFn` resolving consistently through `LdaGlobal`.
    if is_whitelisted_global_name(name) || ctx.is_module_global(name) {
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
            builder
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
            Ok(())
        })();
        ctx.release_temps(argc);
        ctx.release_temps(1);
        return lower;
    }

    Err(SourceLoweringError::unsupported(
        "unbound_function",
        callee_ident.span,
    ))
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
        } else if is_whitelisted_global_name(name) || ctx.is_module_global(name) {
            let idx = ctx.intern_property_name(name)?;
            builder
                .emit(Opcode::LdaGlobal, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaGlobal (spread callee): {err:?}"
                    ))
                })?;
        } else {
            return Err(SourceLoweringError::unsupported(
                "unbound_function",
                callee_ident.span,
            ));
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
        builder
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
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            member.span,
        ));
    }
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
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            member.span,
        ));
    }
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
    if member.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            member.span,
        ));
    }
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
fn enforce_private_name_declared<'a>(
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
fn lower_private_in_expression<'a>(
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
fn lower_private_field_read<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    expr: &'a oxc_ast::ast::PrivateFieldExpression<'a>,
) -> Result<(), SourceLoweringError> {
    if expr.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            expr.span,
        ));
    }
    let name = expr.field.name.as_str();
    enforce_private_name_declared(ctx, name, expr.span)?;
    let base = materialize_member_base(builder, ctx, &expr.object)?;
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
fn enforce_super_property_binding<'a>(
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
fn emit_call_args_and_invoke<'a>(
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
        builder
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
        return Ok(());
    }

    emit_spread_call_arguments_array(builder, ctx, call, args_base)?;
    builder
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
    Ok(())
}

fn emit_spread_call_arguments_array<'a>(
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
/// Rejects spread arguments (`f(...arr)`) with a stable tag so
/// the caller's temp-window accounting stays straight. Shared by
/// the direct-call and method-call paths so the evaluation-order
/// and slot-layout contract is identical.
fn lower_call_arguments_into_temps<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    call: &oxc_ast::ast::CallExpression<'a>,
    base: RegisterIndex,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;
    for (offset, arg) in call.arguments.iter().enumerate() {
        let expr = match arg {
            Argument::SpreadElement(spread) => {
                return Err(SourceLoweringError::unsupported(
                    "spread_call_arg",
                    spread.span,
                ));
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

/// Convert a parsed `NumericLiteral` into an int32. Rejects fractional
/// parts and values outside `i32` range — those surface as
/// `Unsupported { construct: "non_int32_literal" }` because the
/// widening path (`LoadF64` / `LoadBigInt`) lands in a later milestone.
/// Identifies names that always resolve to a runtime-installed
/// global object. Used by both `lower_identifier_reference` and
/// `lower_direct_call` to route the name through `LdaGlobal`
/// instead of rejecting as `unbound_identifier`. Keep in sync
/// with the set of constructors / namespaces the runtime's boot
/// sequence installs — the runtime owns the actual binding, this
/// is just the compiler-side allowlist so we emit
/// runtime-resolvable code.
fn is_whitelisted_global_name(name: &str) -> bool {
    matches!(
        name,
        // Foundation
        "globalThis"
        | "Math"
        | "JSON"
        | "console"
        | "Symbol"
        | "Promise"
        | "Reflect"
        // Core types + constructors
        | "Object"
        | "Array"
        | "String"
        | "Number"
        | "Boolean"
        | "BigInt"
        | "Function"
        | "Date"
        | "RegExp"
        | "Iterator"
        | "AsyncIterator"
        // Error hierarchy
        | "Error"
        | "TypeError"
        | "RangeError"
        | "SyntaxError"
        | "ReferenceError"
        | "EvalError"
        | "URIError"
        | "AggregateError"
        | "SuppressedError"
        // Collections
        | "Map"
        | "Set"
        | "WeakMap"
        | "WeakSet"
        | "WeakRef"
        | "FinalizationRegistry"
        // Binary data
        | "ArrayBuffer"
        | "SharedArrayBuffer"
        | "DataView"
        | "Atomics"
        | "Int8Array"
        | "Uint8Array"
        | "Uint8ClampedArray"
        | "Int16Array"
        | "Uint16Array"
        | "Int32Array"
        | "Uint32Array"
        | "Float32Array"
        | "Float64Array"
        | "BigInt64Array"
        | "BigUint64Array"
        // Proxy
        | "Proxy"
        // Event loop / async
        | "setTimeout"
        | "setInterval"
        | "clearTimeout"
        | "clearInterval"
        | "queueMicrotask"
        // Web APIs (otter-web)
        | "URL"
        | "URLSearchParams"
        | "fetch"
        | "Headers"
        | "Request"
        | "Response"
        | "Blob"
        | "TextEncoder"
        | "TextDecoder"
        | "performance"
        | "structuredClone"
        // Node-compat runtime
        | "process"
        // Host-injected helpers used by the module-graph loader's
        // CJS / ESM source rewrites in `otter-runtime`
        // (`host::module_runtime::transform_*`). User code never
        // references these directly; the transform emits them.
        | "__otter_module"
        | "__otter_cjs_module"
        | "__otter_cjs_url"
        | "require"
        | "module"
        | "exports"
        | "__filename"
        | "__dirname"
    )
}

fn int32_from_literal(literal: &NumericLiteral<'_>) -> Result<i32, SourceLoweringError> {
    let value = literal.value;
    if !value.is_finite() || value.fract() != 0.0 {
        return Err(SourceLoweringError::unsupported(
            "non_int32_literal",
            literal.span,
        ));
    }
    if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) {
        return Err(SourceLoweringError::unsupported(
            "non_int32_literal",
            literal.span,
        ));
    }
    // Safe because `value` is finite, integral, and within i32 range.
    Ok(value as i32)
}

fn expression_construct_tag(expr: &Expression<'_>) -> &'static str {
    match expr {
        Expression::BooleanLiteral(_) => "boolean_literal",
        Expression::NullLiteral(_) => "null_literal",
        Expression::StringLiteral(_) => "string_literal",
        Expression::BigIntLiteral(_) => "bigint_literal",
        Expression::TemplateLiteral(_) => "template_literal",
        Expression::CallExpression(_) => "call_expression",
        Expression::NewExpression(_) => "new_expression",
        Expression::StaticMemberExpression(_) => "member_expression",
        Expression::ComputedMemberExpression(_) => "member_expression",
        Expression::PrivateFieldExpression(_) => "private_field_expression",
        Expression::ArrayExpression(_) => "array_expression",
        Expression::ObjectExpression(_) => "object_expression",
        Expression::FunctionExpression(_) => "function_expression",
        Expression::ArrowFunctionExpression(_) => "arrow_function_expression",
        Expression::ClassExpression(_) => "class_expression",
        Expression::UnaryExpression(_) => "unary_expression",
        Expression::UpdateExpression(_) => "update_expression",
        Expression::LogicalExpression(_) => "logical_expression",
        Expression::ConditionalExpression(_) => "conditional_expression",
        Expression::AssignmentExpression(_) => "assignment_expression",
        Expression::ThisExpression(_) => "this_expression",
        Expression::Super(_) => "super_expression",
        _ => "expression",
    }
}

fn binary_operator_tag(op: BinaryOperator) -> &'static str {
    match op {
        BinaryOperator::Addition => "addition",
        BinaryOperator::Subtraction => "subtraction",
        BinaryOperator::Multiplication => "multiplication",
        BinaryOperator::Division => "division",
        BinaryOperator::Remainder => "remainder",
        BinaryOperator::Exponential => "exponent",
        BinaryOperator::ShiftLeft => "shift_left",
        BinaryOperator::ShiftRight => "shift_right",
        BinaryOperator::ShiftRightZeroFill => "unsigned_shift_right",
        BinaryOperator::BitwiseOR => "bitwise_or",
        BinaryOperator::BitwiseXOR => "bitwise_xor",
        BinaryOperator::BitwiseAnd => "bitwise_and",
        BinaryOperator::Equality
        | BinaryOperator::Inequality
        | BinaryOperator::StrictEquality
        | BinaryOperator::StrictInequality
        | BinaryOperator::LessThan
        | BinaryOperator::LessEqualThan
        | BinaryOperator::GreaterThan
        | BinaryOperator::GreaterEqualThan => "comparison",
        BinaryOperator::In | BinaryOperator::Instanceof => "membership",
    }
}
