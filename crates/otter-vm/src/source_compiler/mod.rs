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
    ConditionalExpression, Expression, FormalParameters, Function, FunctionBody,
    IdentifierReference, LogicalExpression, LogicalOperator, NumericLiteral, ObjectExpression,
    ObjectPropertyKind, Program, PropertyKey, PropertyKind, SimpleAssignmentTarget, Statement,
    StaticMemberExpression, TemplateLiteral, UnaryExpression, UnaryOperator, UpdateExpression,
    UpdateOperator, VariableDeclaration, VariableDeclarationKind, VariableDeclarator,
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
    let body_out = lower_function_body(body, &func.params, &params_layout, function_names)?;

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
        body_out.exceptions,
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
    exceptions: crate::exception::ExceptionTable,
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
    rest_name: Option<&'a str>,
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
/// Rejected with stable tags:
/// - `destructuring_parameter` — any non-identifier pattern.
/// - `rest_destructuring_parameter` — destructuring in a rest.
/// - (The old `multiple_parameters` tag is removed — multi-param
///   signatures are a first-class surface now.)
fn analyze_params<'a>(
    params: &'a FormalParameters<'a>,
) -> Result<ParamsLayout<'a>, SourceLoweringError> {
    let mut names = Vec::with_capacity(params.items.len());
    let mut defaults = Vec::with_capacity(params.items.len());

    for param in params.items.iter() {
        let BindingPattern::BindingIdentifier(ident) = &param.pattern else {
            return Err(SourceLoweringError::unsupported(
                "destructuring_parameter",
                param.span,
            ));
        };
        names.push(ident.name.as_str());
        defaults.push(param.initializer.as_deref());
    }

    // Optional rest parameter. oxc wraps `...rest` in
    // `FormalParameters.rest: FormalParameterRest`, which itself
    // contains a `BindingRestElement { argument: BindingPattern }`.
    let rest_name = if let Some(rest) = params.rest.as_deref() {
        let BindingPattern::BindingIdentifier(ident) = &rest.rest.argument else {
            return Err(SourceLoweringError::unsupported(
                "rest_destructuring_parameter",
                rest.rest.span,
            ));
        };
        Some(ident.name.as_str())
    } else {
        None
    };

    Ok(ParamsLayout {
        names,
        defaults,
        rest_name,
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
    let Some(rest_name) = layout.rest_name else {
        return Ok(());
    };
    // Allocate rest as a `const`-like local. The ES spec treats
    // rest as a fresh binding (not a param alias); using `const`
    // semantics rejects accidental reassignment. Catch-clause /
    // for-init bindings follow the same pattern.
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
    Ok(())
}

fn lower_function_body<'a>(
    body: &'a FunctionBody<'a>,
    _params: &'a FormalParameters<'a>,
    layout: &ParamsLayout<'a>,
    function_names: &'a [&'a str],
) -> Result<FunctionBodyOutput, SourceLoweringError> {
    if !body.directives.is_empty() {
        return Err(SourceLoweringError::unsupported(
            "directive_prologue",
            body.directives[0].span,
        ));
    }

    let mut builder = BytecodeBuilder::new();
    let mut ctx = LoweringContext::new(layout, function_names);

    // §14.1.21 FunctionDeclarationInstantiation — evaluate default
    // initializers for any param whose caller-supplied value is
    // `undefined`, then materialise the rest parameter's array
    // from `activation.overflow_args`. Both run before any body
    // statement so `Ldar r_param` later in the body sees a
    // definite value.
    emit_default_initializers(&mut builder, &mut ctx, layout)?;
    emit_rest_parameter(&mut builder, &mut ctx, layout)?;

    // Split-off for the tail statement. Empty bodies stay rejected
    // since the frame layout still needs some instruction to exit
    // through; a caller could fall through to the synthesized
    // `LdaUndefined; Return` below but callers that pass `{}`
    // typically expect a stronger signal.
    let Some((last, rest)) = body.statements.split_last() else {
        return Err(SourceLoweringError::unsupported("empty_body", body.span));
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
    for stmt in rest {
        lower_top_statement(&mut builder, &mut ctx, stmt)?;
    }
    let needs_synthetic_return = match last {
        Statement::ReturnStatement(ret) if ret.argument.is_some() => {
            let argument = ret.argument.as_ref().expect("checked Some above");
            lower_return_expression(&mut builder, &ctx, argument)?;
            builder
                .emit(Opcode::Return, &[])
                .map_err(|err| SourceLoweringError::Internal(format!("encode Return: {err:?}")))?;
            false
        }
        _ => {
            // Lower the statement (call-statement, assignment, if,
            // while, block, bare `return;`, …) — it must be a
            // shape `lower_top_statement` already accepts.
            lower_top_statement(&mut builder, &mut ctx, last)?;
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
        exceptions: crate::exception::ExceptionTable::new(exception_handlers),
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
        Statement::SwitchStatement(sw) => lower_switch_statement(builder, ctx, sw),
        Statement::ThrowStatement(throw) => lower_throw_statement(builder, ctx, throw),
        Statement::TryStatement(try_stmt) => lower_try_statement(builder, ctx, try_stmt),
        Statement::BreakStatement(break_stmt) => lower_break_statement(builder, ctx, break_stmt),
        Statement::ContinueStatement(cont_stmt) => {
            lower_continue_statement(builder, ctx, cont_stmt)
        }
        Statement::ReturnStatement(ret) => {
            // §14.9 — `return;` returns `undefined`. Bare `return`
            // without an argument lowers to `LdaUndefined; Return`;
            // `return <expr>;` evaluates the expression into acc
            // and exits.
            match ret.argument.as_ref() {
                Some(argument) => {
                    lower_return_expression(builder, ctx, argument)?;
                }
                None => {
                    builder.emit(Opcode::LdaUndefined, &[]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "encode LdaUndefined (bare return): {err:?}"
                        ))
                    })?;
                }
            }
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
        continue_label: Some(loop_header),
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
        continue_label: Some(loop_continue),
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
            });

            let lower_cases = (|| -> Result<(), SourceLoweringError> {
                for (case_idx, case) in sw.cases.iter().enumerate() {
                    builder.bind_label(case_labels[case_idx]).map_err(|err| {
                        SourceLoweringError::Internal(format!(
                            "bind switch case {case_idx}: {err:?}"
                        ))
                    })?;
                    for stmt in case.consequent.iter() {
                        // Case bodies reject `let`/`const` (no
                        // block scoping inside a case yet — the
                        // M12 scoping work treated switch cases
                        // as outside its surface).
                        lower_nested_statement(builder, ctx, stmt)?;
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

/// Lowers `try { … } catch (e) { … } finally { … }`. Supports four
/// shapes — try/catch, try/finally, try/catch/finally, and
/// reject-bare-`try`. Bytecode shape (try/catch/finally — the
/// richest form):
///
/// ```text
///   try_start:
///     <lower try body>
///   try_end:
///     Jump finally_normal          ; normal exit from try → run finally
///   catch_start:
///     LdaException
///     Star r_e                     ; bind catch parameter (if any)
///     <lower catch body>
///   catch_end:
///     Jump finally_normal          ; normal exit from catch → run finally
///   finally_handler:
///     <lower finally body (copy 1)>
///     ReThrow                      ; re-raise after running finally
///   finally_normal:
///     <lower finally body (copy 2)>
///   after_try:
/// ```
///
/// Registered handlers:
/// - `(try_start, try_end, catch_start)` — catches throws from the
///   try body so catch runs.
/// - `(catch_start, catch_end, finally_handler)` — catches throws
///   from inside the catch body so finally still runs, then
///   re-raises.
///
/// For try/catch (no finally), the catch body's end just falls
/// through to `after_try`, and only the first handler is registered.
/// For try/finally (no catch), there's a single handler
/// `(try_start, try_end, finally_handler)` and the try body's
/// normal path jumps directly to `finally_normal`.
///
/// Known simplification (M21): `return` / `break` / `continue` from
/// inside the `try` or `catch` block skips the `finally` body. The
/// spec requires finally to run even on abrupt completions; wiring
/// that needs a deferred-completion mechanism that lands in a
/// later milestone. Normal flow + exception flow are both
/// spec-compliant here.
///
/// §14.15.3 TryStatement, §14.15.4 TryStatement with Finally.
fn lower_try_statement<'a>(
    builder: &mut BytecodeBuilder,
    ctx: &mut LoweringContext<'a>,
    try_stmt: &'a oxc_ast::ast::TryStatement<'a>,
) -> Result<(), SourceLoweringError> {
    if try_stmt.handler.is_none() && try_stmt.finalizer.is_none() {
        // `try { … }` with neither handler nor finalizer is a
        // parse error in real JS; oxc's parser should reject it
        // before we see it, but guard anyway.
        return Err(SourceLoweringError::unsupported(
            "try_without_catch_or_finally",
            try_stmt.span,
        ));
    }

    let try_start = builder.new_label();
    let try_end = builder.new_label();
    let after_try = builder.new_label();

    // Optional labels for the catch and finally chapters.
    let catch_start = try_stmt.handler.as_ref().map(|_| builder.new_label());
    let catch_end = try_stmt.handler.as_ref().map(|_| builder.new_label());
    let finally_handler = try_stmt.finalizer.as_ref().map(|_| builder.new_label());
    let finally_normal = try_stmt.finalizer.as_ref().map(|_| builder.new_label());

    // 1) Try body.
    builder
        .bind_label(try_start)
        .map_err(|err| SourceLoweringError::Internal(format!("bind try_start: {err:?}")))?;
    lower_block_statement(builder, ctx, &try_stmt.block)?;
    builder
        .bind_label(try_end)
        .map_err(|err| SourceLoweringError::Internal(format!("bind try_end: {err:?}")))?;

    // 2) Normal-exit jump out of try — either into finally_normal
    //    (if we have a finalizer) or straight past the handler to
    //    after_try (catch-only shape).
    let try_normal_target = finally_normal.unwrap_or(after_try);
    builder
        .emit_jump_to(Opcode::Jump, try_normal_target)
        .map_err(|err| {
            SourceLoweringError::Internal(format!("encode Jump (try normal exit): {err:?}"))
        })?;

    // 3) Catch block, if present.
    if let (Some(handler), Some(catch_start), Some(catch_end)) =
        (try_stmt.handler.as_deref(), catch_start, catch_end)
    {
        builder
            .bind_label(catch_start)
            .map_err(|err| SourceLoweringError::Internal(format!("bind catch_start: {err:?}")))?;
        // Register handler for the try body.
        ctx.record_exception_handler(try_start, try_end, catch_start);

        // Scope-snapshot around the catch body so the catch
        // parameter `e` pops on catch exit. Without finalizer this
        // is the only scope; with finalizer the binding is still
        // local to the catch body — finally can't see `e`.
        let scope = ctx.snapshot_scope();

        // Bind the catch parameter, if any. `catch { … }` without a
        // param is the "bindingless catch" from ES2019 (§14.15.1).
        let lower_catch = (|| -> Result<(), SourceLoweringError> {
            if let Some(param) = handler.param.as_ref() {
                let BindingPattern::BindingIdentifier(ident) = &param.pattern else {
                    return Err(SourceLoweringError::unsupported(
                        "destructuring_catch_param",
                        param.span,
                    ));
                };
                let name = ident.name.as_str();
                let slot = ctx.allocate_local(name, false, ident.span)?;
                // Pull the pending exception into acc, then bind.
                builder.emit(Opcode::LdaException, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!("encode LdaException: {err:?}"))
                })?;
                builder
                    .emit(Opcode::Star, &[Operand::Reg(u32::from(slot))])
                    .map_err(|err| {
                        SourceLoweringError::Internal(format!("encode Star (catch param): {err:?}"))
                    })?;
                ctx.mark_initialized(name)?;
            } else {
                // Bindingless catch — still need to clear the
                // pending exception from the activation so the
                // next throw/finally path sees a clean slate.
                builder.emit(Opcode::LdaException, &[]).map_err(|err| {
                    SourceLoweringError::Internal(format!(
                        "encode LdaException (bindingless): {err:?}"
                    ))
                })?;
            }
            lower_block_statement(builder, ctx, &handler.body)?;
            Ok(())
        })();
        ctx.restore_scope(scope);
        lower_catch?;

        builder
            .bind_label(catch_end)
            .map_err(|err| SourceLoweringError::Internal(format!("bind catch_end: {err:?}")))?;

        // After catch completes normally, run finally (if any) or
        // jump past.
        builder
            .emit_jump_to(Opcode::Jump, try_normal_target)
            .map_err(|err| {
                SourceLoweringError::Internal(format!("encode Jump (catch normal exit): {err:?}"))
            })?;
    }

    // 4) Finally block, if present.
    if let (Some(finalizer), Some(finally_handler), Some(finally_normal)) = (
        try_stmt.finalizer.as_deref(),
        finally_handler,
        finally_normal,
    ) {
        // Register exception-path handler. If there's a catch,
        // finally catches exceptions from the catch body. If not,
        // finally catches exceptions from the try body directly.
        match (catch_start, catch_end) {
            (Some(cs), Some(ce)) => ctx.record_exception_handler(cs, ce, finally_handler),
            _ => ctx.record_exception_handler(try_start, try_end, finally_handler),
        }

        // 4a) Exception-path finally entry — pending exception is
        //     still set. Emit the finally body, then ReThrow to
        //     re-raise the pending value (which `LdaException` /
        //     `ReThrow` read from the activation).
        builder.bind_label(finally_handler).map_err(|err| {
            SourceLoweringError::Internal(format!("bind finally_handler: {err:?}"))
        })?;
        lower_block_statement(builder, ctx, finalizer)?;
        builder.emit(Opcode::ReThrow, &[]).map_err(|err| {
            SourceLoweringError::Internal(format!("encode ReThrow (finally): {err:?}"))
        })?;

        // 4b) Normal-path finally entry — no pending exception;
        //     run the finally body and fall through to after_try.
        builder.bind_label(finally_normal).map_err(|err| {
            SourceLoweringError::Internal(format!("bind finally_normal: {err:?}"))
        })?;
        lower_block_statement(builder, ctx, finalizer)?;
    }

    builder
        .bind_label(after_try)
        .map_err(|err| SourceLoweringError::Internal(format!("bind after_try: {err:?}")))?;

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
    // `continue` walks past any enclosing `switch` frames (which
    // push break-only labels with `continue_label: None`) to find
    // the innermost frame that actually has a continue target.
    // Spec §14.11 IterationStatement: `continue` binds to the
    // innermost *iteration* statement, not just any break-frame.
    let target = ctx
        .innermost_continue_label()
        .ok_or_else(|| SourceLoweringError::unsupported("continue_outside_loop", cont_stmt.span))?;
    builder
        .emit_jump_to(Opcode::Jump, target)
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

/// Per-function lowering context: tracks parameters (0..N regular
/// plus an optional rest param that lives as a local), every
/// `let`/`const` declared so far (with their assigned register
/// slots and TDZ state), the call-arg temp pool, and the shared
/// module-level function name table for resolving `CallExpression`
/// targets. Scoped declarations push onto `locals` and pop on scope
/// exit while `peak_local_count` retains the high-water mark so the
/// [`FrameLayout`] reserves enough slots for the whole function.
struct LoweringContext<'a> {
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
    /// Pending exception-handler records (M21). Each entry pairs a
    /// try-block's `(try_start_label, try_end_label)` range with the
    /// `handler_label` the interpreter should jump to on an
    /// in-range throw. PCs are resolved out of the builder's label
    /// table after all three labels are bound, just before the
    /// `ExceptionTable` is constructed in
    /// [`lower_function_body`].
    pending_handlers: RefCell<Vec<PendingExceptionHandler>>,
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

/// `break` / `continue` jump targets for one enclosing control
/// frame. `break_label` is bound to the instruction immediately
/// after the loop or switch; `continue_label` is the re-entry
/// point — for `while`, the loop header (re-evaluates the
/// condition); for `for`, the update clause (evaluates the update,
/// then jumps back to the header); for `switch`, `None` since
/// `continue` inside a switch body walks past the switch to the
/// enclosing loop (§14.11).
#[derive(Debug, Clone, Copy)]
struct LoopLabels {
    break_label: Label,
    continue_label: Option<Label>,
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
    fn new(layout: &ParamsLayout<'a>, function_names: &'a [&'a str]) -> Self {
        let param_names = layout.names.clone();
        let param_count = RegisterIndex::try_from(param_names.len()).unwrap_or(u16::MAX);
        Self {
            param_names,
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
            pending_handlers: RefCell::new(Vec::new()),
        }
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
        // Walk params in declaration order; the i-th param lives at
        // user-visible register `i`. Rest-param lookups fall through
        // to the locals search above — rest is a local, not a
        // parameter slot.
        for (i, param) in self.param_names.iter().enumerate() {
            if *param == name {
                let reg = u16::try_from(i)
                    .expect("param index fits in u16 because param_names length does");
                return Some(BindingRef::Param { reg });
            }
        }
        None
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
        "globalThis" | "Math" | "console" => {
            // M14 anchor: `globalThis`, `Math`.
            // M19 anchor: `console` — the "hello world" gate. The
            // runtime already installs a `console` object on the
            // global with `log`/`warn`/`error`/`info`/`debug`
            // bindings backed by the pluggable `ConsoleBackend`
            // trait (`StdioConsoleBackend` is the CLI default).
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
        Expression::TemplateLiteral(tpl) => lower_template_literal(builder, ctx, tpl),
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
                ArrayExpressionElement::Elision(elision) => {
                    return Err(SourceLoweringError::unsupported(
                        "elision_array_element",
                        elision.span,
                    ));
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
        // Parameters are ordinary writable bindings in
        // non-strict mode (§10.2.1 FunctionDeclarationInstantiation
        // puts them on the function's VariableEnvironment with
        // `mutable: true`). Assignment writes back into the
        // parameter slot.
        BindingRef::Param { reg } => reg,
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
                return Err(SourceLoweringError::unsupported(
                    "spread_in_direct_call",
                    call.span,
                ));
            }
            lower_direct_call(builder, ctx, call, ident)
        }
        Expression::StaticMemberExpression(member) => {
            lower_static_method_call(builder, ctx, call, member, has_spread)
        }
        Expression::ComputedMemberExpression(member) => {
            lower_computed_method_call(builder, ctx, call, member, has_spread)
        }
        other => Err(SourceLoweringError::unsupported(
            "non_identifier_callee",
            other.span(),
        )),
    }
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
    let func_idx = ctx
        .resolve_function(callee_ident.name.as_str())
        .ok_or_else(|| SourceLoweringError::unsupported("unbound_function", callee_ident.span))?;

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
            .map_err(|err| SourceLoweringError::Internal(format!("encode CallDirect: {err:?}")))?;
        Ok(())
    })();
    ctx.release_temps(argc);
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

    let lower = (|| -> Result<(), SourceLoweringError> {
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

    let lower = (|| -> Result<(), SourceLoweringError> {
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
    use oxc_ast::ast::Argument;
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

    // Spread path — build an Array of args, then CallSpread.
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
