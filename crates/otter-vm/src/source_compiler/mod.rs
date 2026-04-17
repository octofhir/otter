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
//! - Inside an `if` branch or a `while` body: only assignment
//!   statements, nested `if` statements, `while` statements, `return`
//!   statements, and nested blocks of the same. `let`/`const` inside
//!   any nested block is rejected as `nested_variable_declaration`
//!   until block scoping lands.
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

#[cfg(test)]
mod tests;

pub use error::SourceLoweringError;

use std::cell::{Cell, RefCell};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrayExpression, ArrayExpressionElement, AssignmentExpression, AssignmentOperator,
    AssignmentTarget, BinaryExpression, BinaryOperator, BindingPattern, ComputedMemberExpression,
    ConditionalExpression, Expression, FormalParameter, FormalParameters, Function, FunctionBody,
    IdentifierReference, LogicalExpression, LogicalOperator, NumericLiteral, ObjectExpression,
    ObjectPropertyKind, Program, PropertyKey, PropertyKind, SimpleAssignmentTarget, Statement,
    StaticMemberExpression, UnaryExpression, UnaryOperator, UpdateExpression, UpdateOperator,
    VariableDeclaration, VariableDeclarationKind, VariableDeclarator,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};

use crate::bytecode::{Bytecode, BytecodeBuilder, FeedbackSlot, Label, Opcode, Operand};
use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
use crate::frame::{FrameLayout, RegisterIndex};
use crate::module::{Function as VmFunction, FunctionIndex, FunctionTables, Module};

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

        lower_program(&parser_return.program)
    }
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

fn lower_program(program: &Program<'_>) -> Result<Module, SourceLoweringError> {
    // The program is one or more top-level `FunctionDeclaration`s.
    // Anything else — `class`, `var`, top-level expressions or
    // statements, imports/exports — surfaces as an `Unsupported`
    // pointing at the offending node so later milestones can widen
    // coverage one construct at a time. The conventional `main`
    // pattern (helpers first, entry last) makes the **last**
    // function the module's entry.
    if program.body.is_empty() {
        return Err(SourceLoweringError::unsupported("program", program.span));
    }

    // First pass: collect each declaration's name. Names must be
    // available before any body is lowered so cross-function calls
    // (including forward references and recursion) resolve.
    let mut declarations: Vec<&Function<'_>> = Vec::with_capacity(program.body.len());
    let mut names: Vec<&str> = Vec::with_capacity(program.body.len());
    for stmt in &program.body {
        let func = match stmt {
            Statement::FunctionDeclaration(func) => func.as_ref(),
            Statement::ClassDeclaration(class) => {
                return Err(SourceLoweringError::unsupported(
                    "class_declaration",
                    class.span,
                ));
            }
            other => {
                return Err(SourceLoweringError::unsupported(
                    statement_construct_tag(other),
                    other.span(),
                ));
            }
        };
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
    }

    // Second pass: lower each function with the shared name table
    // available so `f(args)` inside one body can resolve `f` to its
    // `FunctionIndex`.
    let mut functions: Vec<VmFunction> = Vec::with_capacity(declarations.len());
    for func in &declarations {
        functions.push(lower_function_declaration(func, &names)?);
    }

    // Entry = last declared function (conventional `main` lives at
    // the bottom of the file). Safe: `declarations` is non-empty
    // (we returned early above) and `functions.len() ==
    // declarations.len()`.
    let entry_idx = u32::try_from(functions.len() - 1)
        .map_err(|_| SourceLoweringError::Internal("function index overflow".into()))?;

    let module = Module::new(None::<&str>, functions, FunctionIndex(entry_idx)).map_err(|err| {
        SourceLoweringError::Internal(format!("module construction failed: {err}"))
    })?;
    Ok(module)
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

fn lower_function_declaration<'a>(
    func: &'a Function<'a>,
    function_names: &'a [&'a str],
) -> Result<VmFunction, SourceLoweringError> {
    if func.r#async {
        return Err(SourceLoweringError::unsupported(
            "async_function",
            func.span,
        ));
    }
    if func.generator {
        return Err(SourceLoweringError::unsupported("generator", func.span));
    }

    let name = func
        .id
        .as_ref()
        .map(|ident| ident.name.as_str().to_owned())
        .ok_or_else(|| SourceLoweringError::unsupported("anonymous_function", func.span))?;

    let param_count = count_simple_params(&func.params)?;

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
    let body_out = lower_function_body(body, &func.params, param_count, function_names)?;

    // FrameLayout: 1 hidden slot for `this`, then `param_count`
    // parameter slots, then `local_count` `let`/`const` slots, then
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
    let feedback_layout = arithmetic_only_feedback_layout(body_out.feedback_slot_count);
    // M14/M15: wire the accumulated property-name, float-constant,
    // and string-literal interners into the function's side tables
    // so `LdaGlobal` / `LdaConstF64` / `LdaConstStr` can resolve
    // their `Idx` operands at runtime. Other tables (bigints,
    // closures, calls, regexps) stay default-empty until later
    // milestones exercise them.
    let side_tables = crate::module::FunctionSideTables::new(
        body_out.property_names,
        body_out.string_literals,
        body_out.float_constants,
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let tables = FunctionTables::new(
        side_tables,
        feedback_layout,
        Default::default(),
        Default::default(),
        Default::default(),
    );

    Ok(
        VmFunction::new(Some(name), layout, body_out.bytecode, tables)
            .with_strict(func.id.is_some()),
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
    property_names: crate::property::PropertyNameTable,
    float_constants: crate::float::FloatTable,
    string_literals: crate::string::StringTable,
}

/// Build a `FeedbackTableLayout` with `count` [`FeedbackKind::Arithmetic`]
/// slots (ids `0..count`). Source-compiled functions allocate slots in
/// monotonically increasing order, so this direct construction matches
/// the slot ids produced by `LoweringContext::allocate_arithmetic_feedback`.
fn arithmetic_only_feedback_layout(count: u16) -> FeedbackTableLayout {
    let slots: Vec<FeedbackSlotLayout> = (0..count)
        .map(|i| FeedbackSlotLayout::new(FeedbackSlotId(i), FeedbackKind::Arithmetic))
        .collect();
    FeedbackTableLayout::new(slots)
}

fn count_simple_params(
    params: &FormalParameters<'_>,
) -> Result<RegisterIndex, SourceLoweringError> {
    if params.rest.is_some() {
        return Err(SourceLoweringError::unsupported(
            "rest_parameter",
            params.span,
        ));
    }
    match params.items.as_slice() {
        [] => Ok(0),
        [single] => {
            validate_simple_param(single)?;
            Ok(1)
        }
        [_, second, ..] => Err(SourceLoweringError::unsupported(
            "multiple_parameters",
            second.span,
        )),
    }
}

fn validate_simple_param(param: &FormalParameter<'_>) -> Result<(), SourceLoweringError> {
    if param.initializer.is_some() {
        return Err(SourceLoweringError::unsupported(
            "default_parameter",
            param.span,
        ));
    }
    if !matches!(param.pattern, BindingPattern::BindingIdentifier(_)) {
        return Err(SourceLoweringError::unsupported(
            "destructuring_parameter",
            param.span,
        ));
    }
    Ok(())
}

fn lower_function_body<'a>(
    body: &'a FunctionBody<'a>,
    params: &'a FormalParameters<'a>,
    param_count: RegisterIndex,
    function_names: &'a [&'a str],
) -> Result<FunctionBodyOutput, SourceLoweringError> {
    if !body.directives.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "directive_prologue",
            body.directives[0].span,
        ));
    }

    // The function body must end with exactly one `ReturnStatement`
    // — the v2 dispatcher relies on it for tier-up call exits, and
    // M6 has no reachability analysis to synthesize a fall-through
    // `LdaUndefined; Return` for paths that don't return on every
    // branch. Earlier statements are processed via `lower_top_statement`
    // (which accepts top-level `let`/`const` plus everything
    // `lower_nested_statement` accepts).
    let mut builder = BytecodeBuilder::new();
    let mut ctx = LoweringContext::new(params, param_count, function_names);

    let Some((last, rest)) = body.statements.split_last() else {
        return Err(SourceLoweringError::unsupported("empty_body", body.span));
    };

    for stmt in rest {
        lower_top_statement(&mut builder, &mut ctx, stmt)?;
    }

    let Statement::ReturnStatement(ret) = last else {
        return Err(SourceLoweringError::unsupported(
            "missing_return",
            last.span(),
        ));
    };
    let argument = ret
        .argument
        .as_ref()
        .ok_or_else(|| SourceLoweringError::unsupported("return_without_value", ret.span))?;
    lower_return_expression(&mut builder, &ctx, argument)?;
    builder
        .emit(Opcode::Return, &[])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;

    let bytecode = builder
        .finish()
        .map_err(|err| SourceLoweringError::Internal(format!("finalise bytecode: {err:?}")))?;

    Ok(FunctionBodyOutput {
        bytecode,
        local_count: ctx.local_count(),
        temp_count: ctx.temp_count(),
        feedback_slot_count: ctx.feedback_slot_count(),
        property_names: ctx.take_property_names(),
        float_constants: ctx.take_float_constants(),
        string_literals: ctx.take_string_literals(),
    })
}

/// Lowers a single statement at function-body top level. Accepts the
/// full M6 statement surface, including `let`/`const` declarations
/// (which are not allowed inside nested blocks — those go through
/// [`lower_nested_statement`] instead).
fn lower_top_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    match stmt {
        Statement::VariableDeclaration(decl) => lower_let_const_declaration(builder, ctx, decl),
        _ => lower_nested_statement(builder, ctx, stmt),
    }
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
fn lower_nested_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    stmt: &'a Statement<'a>,
) -> Result<(), SourceLoweringError> {
    match stmt {
        Statement::ExpressionStatement(expr_stmt) => {
            // Statement-position expressions: `AssignmentExpression`
            // (`x = …;`), `CallExpression` (`f();`),
            // `UpdateExpression` (`x++;` — writes x back, result
            // value discarded). The last is the canonical
            // loop-counter idiom. Bare reads, member access, etc.
            // surface their own tag so a future milestone can widen
            // them one shape at a time.
            match &expr_stmt.expression {
                Expression::AssignmentExpression(assign) => {
                    lower_assignment_expression(builder, ctx, assign)
                }
                Expression::CallExpression(call) => lower_call_expression(builder, ctx, call),
                Expression::UpdateExpression(update) => {
                    lower_update_expression(builder, ctx, update)
                }
                other => Err(SourceLoweringError::unsupported(
                    expression_construct_tag(other),
                    other.span(),
                )),
            }
        }
        Statement::IfStatement(if_stmt) => lower_if_statement(builder, ctx, if_stmt),
        Statement::WhileStatement(while_stmt) => lower_while_statement(builder, ctx, while_stmt),
        Statement::ForStatement(for_stmt) => lower_for_statement(builder, ctx, for_stmt),
        Statement::BreakStatement(break_stmt) => lower_break_statement(builder, ctx, break_stmt),
        Statement::ContinueStatement(cont_stmt) => {
            lower_continue_statement(builder, ctx, cont_stmt)
        }
        Statement::ReturnStatement(ret) => {
            let argument = ret.argument.as_ref().ok_or_else(|| {
                SourceLoweringError::unsupported("return_without_value", ret.span)
            })?;
            lower_return_expression(builder, ctx, argument)?;
            builder
                .emit(Opcode::Return, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
            Ok(())
        }
        Statement::BlockStatement(block) => lower_block_statement(builder, ctx, block),
        Statement::VariableDeclaration(decl) => Err(SourceLoweringError::unsupported(
            "nested_variable_declaration",
            decl.span,
        )),
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
    let mut result = Ok(());
    for inner in &block.body {
        let step = match inner {
            Statement::VariableDeclaration(decl) => lower_let_const_declaration(builder, ctx, decl),
            _ => lower_nested_statement(builder, ctx, inner),
        };
        if let Err(err) = step {
            result = Err(err);
            break;
        }
    }
    ctx.restore_scope(scope);
    result
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
        continue_label: loop_header,
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
            other => {
                return Err(SourceLoweringError::unsupported(
                    "for_init_expression",
                    other.span(),
                ));
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
        continue_label: loop_continue,
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
            other => {
                return Err(SourceLoweringError::unsupported(
                    "for_update_expression",
                    other.span(),
                ));
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

/// Lowers `break;` → `Jump loop_exit` for the innermost enclosing
/// loop.
///
/// Labelled breaks (`break outer;`) are rejected with a stable
/// `labelled_break` tag; the label-tracking plumbing lands with
/// broader labelled-statement support (M11+). A bare `break`
/// outside any loop surfaces as `break_outside_loop` so users get a
/// clear compile-time diagnostic instead of a silent fall-through.
fn lower_break_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    break_stmt: &'a oxc_ast::ast::BreakStatement<'a>,
) -> Result<(), SourceLoweringError> {
    if break_stmt.label.is_some() {
        return Err(SourceLoweringError::unsupported(
            "labelled_break",
            break_stmt.span,
        ));
    }
    let labels = ctx
        .innermost_loop_labels()
        .ok_or_else(|| SourceLoweringError::unsupported("break_outside_loop", break_stmt.span))?;
    builder
        .emit_jump_to(Opcode::Jump, labels.break_label)
        .map_err(|err| SourceLoweringError::Internal(format!("encode Jump (break): {err:?}")))?;
    Ok(())
}

/// Lowers `continue;` → `Jump continue_label` for the innermost
/// enclosing loop.
///
/// For `while`, `continue_label` is the loop header (the test
/// re-runs). For `for`, it's the update clause (which then falls
/// through to the loop header). Labelled continues and continue
/// outside a loop surface their own stable tags for the same
/// reasons as [`lower_break_statement`].
fn lower_continue_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'a>,
    cont_stmt: &'a oxc_ast::ast::ContinueStatement<'a>,
) -> Result<(), SourceLoweringError> {
    if cont_stmt.label.is_some() {
        return Err(SourceLoweringError::unsupported(
            "labelled_continue",
            cont_stmt.span,
        ));
    }
    let labels = ctx
        .innermost_loop_labels()
        .ok_or_else(|| SourceLoweringError::unsupported("continue_outside_loop", cont_stmt.span))?;
    builder
        .emit_jump_to(Opcode::Jump, labels.continue_label)
        .map_err(|err| SourceLoweringError::Internal(format!("encode Jump (continue): {err:?}")))?;
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
}

/// Per-function lowering context: tracks the sole parameter (if any)
/// plus every `let`/`const` declared so far (with their assigned
/// register slots and TDZ state), the call-arg temp pool, and the
/// shared module-level function name table for resolving
/// `CallExpression` targets. Scoped declarations (currently only
/// `for` init `let`s) push onto `locals` and pop on scope exit while
/// `peak_local_count` retains the high-water mark so the
/// [`FrameLayout`] reserves enough slots for the whole function.
struct LoweringContext<'a> {
    param_name: Option<&'a str>,
    /// Number of parameter slots in the frame, used to compute the
    /// next local slot index (`param_count + locals.len()`).
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
}

/// `break` / `continue` jump targets for one loop frame. `break_label`
/// is bound to the instruction immediately after the loop; `continue_label`
/// is the re-entry point — for `while`, the loop header (which re-
/// evaluates the condition); for `for`, the update clause (which
/// evaluates the update, then jumps back to the header).
#[derive(Debug, Clone, Copy)]
struct LoopLabels {
    break_label: Label,
    continue_label: Label,
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
        params: &'a FormalParameters<'a>,
        param_count: RegisterIndex,
        function_names: &'a [&'a str],
    ) -> Self {
        let param_name = match params.items.as_slice() {
            [single] => match &single.pattern {
                BindingPattern::BindingIdentifier(ident) => Some(ident.name.as_str()),
                _ => None,
            },
            _ => None,
        };
        Self {
            param_name,
            param_count,
            locals: Vec::new(),
            peak_local_count: 0,
            current_temp_count: Cell::new(0),
            peak_temp_count: Cell::new(0),
            function_names,
            next_feedback_slot: Cell::new(0),
            loop_labels: RefCell::new(Vec::new()),
            scope_starts: RefCell::new(Vec::new()),
            property_names: RefCell::new(Vec::new()),
            float_constants: RefCell::new(Vec::new()),
            string_literals: RefCell::new(Vec::new()),
        }
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

    /// Push a fresh [`LoopLabels`] frame onto the stack. Paired 1:1
    /// with [`Self::exit_loop`] — `lower_while_statement` and
    /// `lower_for_statement` always pop before returning to their
    /// caller.
    fn enter_loop(&self, labels: LoopLabels) {
        self.loop_labels.borrow_mut().push(labels);
    }

    /// Pop the most-recent [`LoopLabels`] frame. Panics in
    /// `debug_assertions` if the stack is empty, because that would
    /// mean an unbalanced `enter_loop` / `exit_loop` pair — a
    /// programmer error the emitter wants to catch eagerly.
    fn exit_loop(&self) {
        let popped = self.loop_labels.borrow_mut().pop();
        debug_assert!(popped.is_some(), "exit_loop called without enter_loop");
    }

    /// Returns the innermost loop's [`LoopLabels`], if any. `None`
    /// means we're currently lowering code outside every loop — the
    /// statement handlers use this to surface `break_outside_loop` /
    /// `continue_outside_loop` errors.
    fn innermost_loop_labels(&self) -> Option<LoopLabels> {
        self.loop_labels.borrow().last().copied()
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
        FeedbackSlot(id)
    }

    /// Current count of allocated arithmetic-feedback slots. Consumed
    /// by [`lower_function_body`] to build the function's
    /// [`FeedbackTableLayout`].
    fn feedback_slot_count(&self) -> u16 {
        self.next_feedback_slot.get()
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
        let scope_start = self.scope_starts.borrow().last().copied().unwrap_or(0);
        let same_scope_duplicate = self.locals[scope_start..].iter().any(|l| l.name == name);
        // Parameters live in the function's outermost lexical scope,
        // so they collide with a top-scope `let`/`const` of the same
        // name but NOT with a same-named binding inside a nested
        // block.
        let param_collision = scope_start == 0 && self.param_name == Some(name);
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

    /// Resolves a JS identifier reference into a [`BindingRef`].
    /// Locals shadow the parameter (only matters once shadowing is
    /// allowed in later milestones; at M5 `allocate_local` rejects
    /// duplicates so the lookup is unambiguous).
    fn resolve_identifier(&self, name: &str) -> Option<BindingRef> {
        if let Some(local) = self.locals.iter().rev().find(|l| l.name == name) {
            return Some(BindingRef::Local {
                reg: local.slot,
                initialized: local.initialized,
                is_const: local.is_const,
            });
        }
        match self.param_name {
            Some(param) if param == name => Some(BindingRef::Param { reg: 0 }),
            _ => None,
        }
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
fn lower_let_const_declaration<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    decl: &'a VariableDeclaration<'a>,
) -> Result<(), SourceLoweringError> {
    let is_const = match decl.kind {
        VariableDeclarationKind::Let => false,
        VariableDeclarationKind::Const => true,
        VariableDeclarationKind::Var => {
            return Err(SourceLoweringError::unsupported(
                "var_declaration",
                decl.span,
            ));
        }
        // `using` / `await using` (Stage 3 explicit resource management).
        // Not on the M5 surface — surface a stable tag so later milestones
        // can pick it up without churning callers.
        VariableDeclarationKind::Using | VariableDeclarationKind::AwaitUsing => {
            return Err(SourceLoweringError::unsupported(
                "using_declaration",
                decl.span,
            ));
        }
    };

    if decl.declarations.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "empty_variable_declaration",
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
    let name = match &declarator.id {
        BindingPattern::BindingIdentifier(ident) => ident.name.as_str(),
        _ => {
            return Err(SourceLoweringError::unsupported(
                "destructuring_binding",
                declarator.span,
            ));
        }
    };

    let init = declarator.init.as_ref().ok_or_else(|| {
        SourceLoweringError::unsupported("uninitialized_binding", declarator.span)
    })?;

    let slot = ctx.allocate_local(name, is_const, declarator.span)?;

    // Lower the init into the accumulator. Reading the binding inside
    // its own initializer hits the `Local { initialized: false }` arm
    // of `lower_identifier_read` and surfaces as `tdz_self_reference`.
    lower_return_expression(builder, ctx, init)?;

    // Persist the init result into the local's slot.
    builder
        .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Star: {err:?}")))?;

    ctx.mark_initialized(name)
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
        "globalThis" | "Math" => {
            let idx = ctx.intern_property_name(name)?;
            builder
                .emit(Opcode::LdaGlobal, &[Operand::Idx(idx)])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaGlobal: {err:?}"))
                })?;
            Ok(())
        }
        _ => Err(SourceLoweringError::unsupported(
            "unbound_identifier",
            ident.span,
        )),
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
    let reg = match binding {
        BindingRef::Param { reg } => reg,
        BindingRef::Local {
            reg,
            initialized: true,
            ..
        } => reg,
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident_span,
            ));
        }
    };
    let pc = builder
        .emit(Opcode::Ldar, &[Operand::Reg(u32::from(reg))])
        .map_err(|err| SourceLoweringError::Internal(format!("encode Ldar: {err:?}")))?;
    let slot = ctx.allocate_arithmetic_feedback();
    builder.attach_feedback(pc, slot);
    Ok(())
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

fn lower_return_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &Expression<'_>,
) -> Result<(), SourceLoweringError> {
    match expr {
        Expression::Identifier(ident) => lower_identifier_reference(builder, ctx, ident),
        Expression::NumericLiteral(literal) => {
            let value = int32_from_literal(literal)?;
            builder
                .emit(Opcode::LdaSmi, &[Operand::Imm(value)])
                .map_err(|err| SourceLoweringError::Internal(format!("encode LdaSmi: {err:?}")))?;
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
        other => Err(SourceLoweringError::unsupported(
            expression_construct_tag(other),
            other.span(),
        )),
    }
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
            return Err(SourceLoweringError::unsupported("delete_unary", expr.span));
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
        BindingRef::Param { .. } => {
            return Err(SourceLoweringError::unsupported(
                "update_on_param",
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
                ObjectPropertyKind::SpreadProperty(s) => {
                    return Err(SourceLoweringError::unsupported(
                        "object_spread_property",
                        s.span,
                    ));
                }
            };
            if prop.computed {
                return Err(SourceLoweringError::unsupported(
                    "computed_property_key",
                    prop.span,
                ));
            }
            if prop.method {
                return Err(SourceLoweringError::unsupported(
                    "method_property",
                    prop.span,
                ));
            }
            if prop.shorthand {
                return Err(SourceLoweringError::unsupported(
                    "shorthand_property",
                    prop.span,
                ));
            }
            if !matches!(prop.kind, PropertyKind::Init) {
                return Err(SourceLoweringError::unsupported(
                    "accessor_property",
                    prop.span,
                ));
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
            // Lower the value into acc; the object handle is
            // safely spilled.
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
            let element_expr = match element {
                ArrayExpressionElement::SpreadElement(spread) => {
                    return Err(SourceLoweringError::unsupported(
                        "spread_array_element",
                        spread.span,
                    ));
                }
                ArrayExpressionElement::Elision(elision) => {
                    return Err(SourceLoweringError::unsupported(
                        "elision_array_element",
                        elision.span,
                    ));
                }
                // Non-spread, non-hole element. `to_expression`
                // downcasts the `Expression` variants inlined by
                // `ArrayExpressionElement` back to `&Expression`.
                other => other.to_expression(),
            };
            lower_return_expression(builder, ctx, element_expr)?;
            builder
                .emit(Opcode::ArrayPush, &[Operand::Reg(u32::from(arr_temp))])
                .map_err(|err| {
                    SourceLoweringError::Internal(format!("encode ArrayPush: {err:?}"))
                })?;
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
        let reg = match binding {
            BindingRef::Param { reg } => reg,
            BindingRef::Local {
                reg,
                initialized: true,
                ..
            } => reg,
            BindingRef::Local {
                initialized: false, ..
            } => {
                return Err(SourceLoweringError::unsupported(
                    "tdz_self_reference",
                    ident.span,
                ));
            }
        };
        return Ok(MemberBase { reg, temp_count: 0 });
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
/// Optional chaining (`o?.x`) is rejected — it requires the nullish
/// short-circuit wiring that lands in a later milestone.
///
/// §13.3.2 Property Accessors
/// <https://tc39.es/ecma262/#sec-property-accessors>
fn lower_static_member_read(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    expr: &StaticMemberExpression<'_>,
) -> Result<(), SourceLoweringError> {
    if expr.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            expr.span,
        ));
    }
    let base = materialize_member_base(builder, ctx, &expr.object)?;
    let idx = ctx.intern_property_name(expr.property.name.as_str())?;
    builder
        .emit(
            Opcode::LdaNamedProperty,
            &[Operand::Reg(u32::from(base.reg)), Operand::Idx(idx)],
        )
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode LdaNamedProperty: {err:?}"))
        })?;
    if base.temp_count != 0 {
        ctx.release_temps(base.temp_count);
    }
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
    if expr.optional {
        return Err(SourceLoweringError::unsupported(
            "optional_member_expression",
            expr.span,
        ));
    }
    let base = materialize_member_base(builder, ctx, &expr.object)?;
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
        // Anything else (literal-literal, paren, nested binary, …) —
        // a future milestone with scratch slots can extend this.
        _ => {
            return Err(SourceLoweringError::unsupported(
                "relational_needs_register_operand",
                expr.span,
            ));
        }
    };

    match direction {
        Direction::Forward { rhs_ident } => {
            lower_accumulator_operand(builder, ctx, &expr.left)?;
            let binding = ctx
                .resolve_identifier(rhs_ident.name.as_str())
                .ok_or_else(|| {
                    SourceLoweringError::unsupported("unbound_identifier", rhs_ident.span)
                })?;
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
    let reg = match binding {
        BindingRef::Param { reg } => reg,
        BindingRef::Local {
            reg,
            initialized: true,
            ..
        } => reg,
        BindingRef::Local {
            initialized: false, ..
        } => {
            return Err(SourceLoweringError::unsupported(
                "tdz_self_reference",
                ident_span,
            ));
        }
    };
    let pc = builder
        .emit(opcode, &[Operand::Reg(u32::from(reg))])
        .map_err(|err| SourceLoweringError::Internal(format!("encode {label}: {err:?}")))?;
    let slot = ctx.allocate_arithmetic_feedback();
    builder.attach_feedback(pc, slot);
    Ok(pc)
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
            let value = int32_from_literal(literal)?;
            let fits_i8 = (i32::from(i8::MIN)..=i32::from(i8::MAX)).contains(&value);
            match (encoding.smi_opcode, fits_i8) {
                (Some(smi_op), true) => {
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
                }
                _ => {
                    return Err(SourceLoweringError::unsupported(
                        "wide_integer_literal_on_rhs",
                        literal.span,
                    ));
                }
            }
            Ok(())
        }
        Expression::Identifier(ident) => {
            let binding = ctx.resolve_identifier(ident.name.as_str()).ok_or_else(|| {
                SourceLoweringError::unsupported("unbound_identifier", ident.span)
            })?;
            lower_identifier_as_reg_rhs(builder, ctx, encoding, binding, ident.span)
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
        | Expression::ComputedMemberExpression(_) => {
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
/// - parameter as target → `assignment_to_param`;
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
        AssignmentTarget::PrivateFieldExpression(member) => Err(SourceLoweringError::unsupported(
            "private_field_assignment_target",
            member.span,
        )),
        AssignmentTarget::ArrayAssignmentTarget(pattern) => Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            pattern.span,
        )),
        AssignmentTarget::ObjectAssignmentTarget(pattern) => Err(SourceLoweringError::unsupported(
            "destructuring_assignment_target",
            pattern.span,
        )),
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
    let target_reg = match binding {
        BindingRef::Local {
            reg,
            initialized: true,
            is_const: false,
        } => reg,
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
        BindingRef::Param { .. } => {
            return Err(SourceLoweringError::unsupported(
                "assignment_to_param",
                target_span,
            ));
        }
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
        let ldar_pc = builder
            .emit(Opcode::Ldar, &[Operand::Reg(u32::from(target_reg))])
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Ldar (compound lhs): {err:?}"))
            })?;
        let ldar_slot = ctx.allocate_arithmetic_feedback();
        builder.attach_feedback(ldar_pc, ldar_slot);
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

/// Maps a compound assignment operator to the binary operator whose
/// encoding it should use. Returns `None` for `=` (handled separately,
/// no underlying binary op) and for compound forms outside the M5
/// surface (`/=`, `%=`, `**=`, `<<=`, `>>=`, `>>>=`, `&=`, `^=`,
/// `||=`, `&&=`, `??=`).
fn compound_assign_to_binary_operator(op: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    use BinaryOperator as B;
    Some(match op {
        A::Addition => B::Addition,
        A::Subtraction => B::Subtraction,
        A::Multiplication => B::Multiplication,
        A::BitwiseOR => B::BitwiseOR,
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

/// Lowers a `CallExpression` whose callee is the name of a
/// top-level `FunctionDeclaration` in the same module. Bytecode
/// shape:
///
/// ```text
///   <lower arg 0> ; Star r_temp0
///   <lower arg 1> ; Star r_temp1
///   …
///   CallDirect func_idx, RegList { base: temp0, count: argc }
/// ```
///
/// The call result lands in the accumulator. Temps are acquired
/// from the function-level pool ([`LoweringContext::acquire_temps`])
/// so nested calls (e.g. `f(g(1, 2), h(3))`) get their own
/// non-overlapping windows; the pool is released LIFO when the call
/// completes.
///
/// Rejects:
/// - non-identifier callee (member, computed, …) →
///   `non_identifier_callee`;
/// - identifier callee that doesn't name a top-level function →
///   `unbound_function` (parameters and locals can't be called yet —
///   closure values land later);
/// - spread args (`f(...arr)`) → `spread_call_arg`;
/// - `new f()` is a separate AST node (`NewExpression`) and falls
///   through to `expression_construct_tag`.
fn lower_call_expression(
    builder: &mut BytecodeBuilder,
    ctx: &LoweringContext<'_>,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> Result<(), SourceLoweringError> {
    use oxc_ast::ast::Argument;

    // 1) Resolve callee.
    let callee_ident = match &call.callee {
        Expression::Identifier(ident) => ident,
        Expression::ParenthesizedExpression(inner) => match &inner.expression {
            Expression::Identifier(ident) => ident,
            other => {
                return Err(SourceLoweringError::unsupported(
                    "non_identifier_callee",
                    other.span(),
                ));
            }
        },
        other => {
            return Err(SourceLoweringError::unsupported(
                "non_identifier_callee",
                other.span(),
            ));
        }
    };
    let func_idx = ctx
        .resolve_function(callee_ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::unsupported("unbound_function", callee_ident.span))?;

    // 2) Acquire temp slots for the argument window.
    let argc = RegisterIndex::try_from(call.arguments.len())
        .map_err(|_| SourceLoweringError::Internal("call argument count exceeds u16".into()))?;
    let base = ctx.acquire_temps(argc)?;

    // 3) Lower each arg into its temp slot. Spread args are
    //    rejected — they need iterator protocol + dynamic argc.
    for (offset, arg) in call.arguments.iter().enumerate() {
        let expr = match arg {
            Argument::SpreadElement(spread) => {
                ctx.release_temps(argc);
                return Err(SourceLoweringError::unsupported(
                    "spread_call_arg",
                    spread.span,
                ));
            }
            // Non-spread argument: the `Argument` enum inlines all
            // `Expression` variants, so we can downcast to `&Expression`
            // and reuse the standard expression-lowering path.
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

    // 4) Emit CallDirect — function index + the contiguous arg window.
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
        .map_err(|err| SourceLoweringError::Internal(format!("encode CallDirect: {err:?}")))?;

    // 5) Release the temp slots (LIFO with respect to nested
    //    calls). The slots stay reserved by the FrameLayout's
    //    `temporary_count` (which tracks the peak, not the live
    //    count) so a later call at the same depth reuses them.
    ctx.release_temps(argc);

    Ok(())
}

/// Convert a parsed `NumericLiteral` into an int32. Rejects fractional
/// parts and values outside `i32` range — those surface as
/// `Unsupported { construct: "non_int32_literal" }` because the
/// widening path (`LoadF64` / `LoadBigInt`) lands in a later milestone.
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
