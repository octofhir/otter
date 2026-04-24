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

mod assignment_targets;
mod assignments;
mod binary_ops;
mod calls;
mod classes;
mod declarations;
mod direct_calls;
mod error;
mod expressions;
mod for_in_of;
mod functions;
mod identifiers;
mod logical_assignment;
mod member_access;
mod optional_calls;
mod statements;
mod switch_scope;
mod toplevel;
mod try_finally;
mod updates;
mod using_decl;

#[cfg(test)]
mod tests;

pub use error::SourceLoweringError;

use std::cell::{Cell, RefCell};

use assignment_targets::{AssignmentTargetRef, unwrap_assignment_target};
use assignments::{
    assign_computed_member, assign_static_member, destructure_array_assignment_from_temp,
    destructure_array_assignment_from_temp_indexed, destructure_object_assignment_from_temp,
    lower_assignment_expression,
};
use binary_ops::{
    BinaryOpEncoding, apply_binary_op_with_acc_lhs, binary_op_encoding,
    emit_identifier_as_reg_operand, lower_binary_expression,
};
use calls::{
    emit_call_args_and_invoke, emit_spread_call_arguments_array, enforce_private_name_declared,
    enforce_super_property_binding, lower_call_arguments_into_temps, lower_call_expression,
    lower_private_field_read, lower_private_in_expression,
};
use classes::{lower_class_expression, lower_nested_class_declaration};
use declarations::{
    emit_array_rest_slice, emit_default_for_destructured_leaf, emit_object_rest_copy,
    emit_string_literal_to_register, lower_let_const_declaration, lower_pattern_bind,
};
use expressions::{
    lower_array_expression, lower_conditional_expression, lower_logical_expression,
    lower_object_expression, lower_tagged_template_expression, lower_template_literal,
    lower_unary_expression, numeric_literal_property_key, property_key_tag,
};
use functions::{
    ParamsLayout, analyze_params, feedback_layout_from_kinds, lower_arrow_function_expression,
    lower_function_body, lower_function_expression, lower_inner_callable_with_super,
    lower_nested_function_declaration, lower_new_expression,
};
use identifiers::{
    emit_assert_binding_ready_for_write, emit_load_binding_value, lower_accumulator_operand,
    lower_identifier_as_reg_rhs, lower_identifier_read, lower_return_expression,
};
use member_access::{
    emit_optional_nullish_short_circuit, lower_computed_member_read, lower_static_member_read,
    materialize_member_base, optional_member_short_circuit,
};
use statements::{lower_block_statement, lower_nested_statement, lower_top_statement};
use toplevel::{
    MODULE_DEFAULT_EXPORT_LOCAL, MODULE_GLOBALS_OVERRIDE, SOURCE_INDEX_OVERRIDE, lower_program,
    statement_construct_tag,
};
use direct_calls::lower_expression_direct_call;
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
    ObjectPropertyKind, Program, PropertyKey, PropertyKind, Statement, StaticMemberExpression,
    TemplateLiteral, UnaryExpression, UnaryOperator, UpdateExpression, UpdateOperator,
    VariableDeclaration, VariableDeclarationKind, VariableDeclarator,
};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use switch_scope::{
    enter_switch_lexical_scope, hoist_switch_var_declarations, lower_switch_case_statement,
};
use try_finally::{
    lower_break_statement, lower_continue_statement, lower_return_statement, lower_try_statement,
};
use updates::lower_update_expression;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TopLevelCompletion {
    Undefined,
    LastExpressionStatement,
}

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
        self.compile_with_completion(
            source,
            source_url,
            source_type,
            TopLevelCompletion::Undefined,
        )
    }

    /// Parse and lower `source` for `eval`, preserving the completion
    /// value of a trailing expression statement.
    pub fn compile_eval(
        &self,
        source: &str,
        source_url: &str,
        source_type: SourceType,
    ) -> Result<Module, SourceLoweringError> {
        self.compile_with_completion(
            source,
            source_url,
            source_type,
            TopLevelCompletion::LastExpressionStatement,
        )
    }

    fn compile_with_completion(
        &self,
        source: &str,
        source_url: &str,
        source_type: SourceType,
        completion: TopLevelCompletion,
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
        let result = lower_program(&parser_return.program, completion);
        SOURCE_INDEX_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = None;
        });
        result
    }
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
    /// order). `is_const` mirrors the original binding so write
    /// guards still work across closure boundaries.
    Upvalue {
        idx: u16,
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
    is_var: bool,
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
    /// §9.1.1.4: `true` when this context is the synthesised top-
    /// level entry's body; each `var`/`let`/`const NAME = init;`
    /// tracked as a module-global is mirrored onto `globalThis.NAME`
    /// right after its local store so a nested call made mid-body
    /// reads the freshly-bound value via `LdaGlobal`. Off by default
    /// — only the top-level entry flips it on.
    mirror_top_level_decls_to_global: Cell<bool>,
}

/// §13.3.7 / §15.7.14 — per-function metadata describing which
/// forms of `super` are syntactically valid inside the function's
/// body.
#[derive(Debug, Clone, Copy)]
pub(super) struct ClassSuperBinding {
    /// `super.x` / `super[k]` / `super.m(args)` — allowed for any
    /// class method / getter / setter / constructor. Gated by the
    /// presence of `[[HomeObject]]` on the active closure.
    pub(super) allow_super_property: bool,
    /// `super(args)` — allowed only in derived-class constructors.
    pub(super) allow_super_call: bool,
}

#[derive(Debug, Clone)]
struct CaptureEntry {
    name: String,
    descriptor: crate::closure::CaptureDescriptor,
    is_const: bool,
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
            mirror_top_level_decls_to_global: Cell::new(false),
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

    /// When `true`, every `var`/`let`/`const NAME = init;` that maps
    /// to a top-level module-global gets its value mirrored onto
    /// `globalThis.NAME` at the binding site (via `StaGlobal`) so
    /// nested calls invoked mid-body see the value. Set only on the
    /// synthesised top-level entry; nested contexts inherit `false`.
    fn enable_top_level_global_mirroring(&self) {
        self.mirror_top_level_decls_to_global.set(true);
    }

    /// Returns `true` when the current binding site should mirror to
    /// `globalThis` — i.e. mirroring is enabled for this context AND
    /// the name is already tracked as a module-global.
    fn should_mirror_top_level_decl_to_global(&self, name: &str) -> bool {
        self.mirror_top_level_decls_to_global.get() && self.is_module_global(name)
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

    /// C4: allocate a [`FeedbackKind::Comparison`] slot for a
    /// `Test*` opcode. The dispatcher records an observation per
    /// execution; the monotonic lattice
    /// (`None → Int32 → Number → String → Any`) lets downstream
    /// consumers (tier-2 JIT, IC specialization) speculate on the
    /// observed operand type.
    fn allocate_comparison_feedback(&self) -> FeedbackSlot {
        let id = self.next_feedback_slot.get();
        debug_assert!(
            id < u16::MAX,
            "feedback slot counter overflow — pathological function > 65 535 feedback ops",
        );
        self.next_feedback_slot.set(id.saturating_add(1));
        self.feedback_slot_kinds
            .borrow_mut()
            .push(FeedbackKind::Comparison);
        FeedbackSlot(id)
    }

    /// C4: allocate a [`FeedbackKind::Branch`] slot for a
    /// conditional jump. The dispatcher records
    /// taken/not-taken saturating counters so downstream
    /// optimizers can layout the hot side through the fallthrough
    /// path.
    fn allocate_branch_feedback(&self) -> FeedbackSlot {
        let id = self.next_feedback_slot.get();
        debug_assert!(
            id < u16::MAX,
            "feedback slot counter overflow — pathological function > 65 535 feedback ops",
        );
        self.next_feedback_slot.set(id.saturating_add(1));
        self.feedback_slot_kinds
            .borrow_mut()
            .push(FeedbackKind::Branch);
        FeedbackSlot(id)
    }

    /// C4: allocate a [`FeedbackKind::Call`] slot for a call
    /// instruction. The dispatcher records the target
    /// `FunctionIndex` each call; monomorphic observations let
    /// tier-2 inline the callee.
    fn allocate_call_feedback(&self) -> FeedbackSlot {
        let id = self.next_feedback_slot.get();
        debug_assert!(
            id < u16::MAX,
            "feedback slot counter overflow — pathological function > 65 535 feedback ops",
        );
        self.next_feedback_slot.set(id.saturating_add(1));
        self.feedback_slot_kinds
            .borrow_mut()
            .push(FeedbackKind::Call);
        FeedbackSlot(id)
    }

    /// C4: allocate + attach a comparison feedback slot at `pc`.
    /// Bundles the common two-line pattern used at every `Test*`
    /// emission site.
    fn attach_comparison_feedback(&self, builder: &mut BytecodeBuilder, pc: u32) {
        let slot = self.allocate_comparison_feedback();
        builder.attach_feedback(pc, slot);
    }

    /// C4: allocate + attach a branch feedback slot at `pc`.
    fn attach_branch_feedback(&self, builder: &mut BytecodeBuilder, pc: u32) {
        let slot = self.allocate_branch_feedback();
        builder.attach_feedback(pc, slot);
    }

    /// C4: allocate + attach a call feedback slot at `pc`.
    fn attach_call_feedback(&self, builder: &mut BytecodeBuilder, pc: u32) {
        let slot = self.allocate_call_feedback();
        builder.attach_feedback(pc, slot);
    }

    /// C4: allocate + attach a property feedback slot at `pc`.
    /// Used for `StaNamedProperty` store sites so the store IC
    /// can specialize on shape like the load IC already does.
    fn attach_property_store_feedback(&self, builder: &mut BytecodeBuilder, pc: u32) {
        let slot = self.allocate_property_feedback();
        builder.attach_feedback(pc, slot);
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
        self.allocate_local_with_mode(name, is_const, false, false, span)
    }

    fn allocate_var_local(
        &mut self,
        name: &'a str,
        span: Span,
    ) -> Result<(u16, bool), SourceLoweringError> {
        let scope_start = self.scope_starts.borrow().last().copied().unwrap_or(0);
        if let Some(local) = self.locals[scope_start..]
            .iter()
            .rev()
            .find(|local| local.name == name)
        {
            if local.is_var && !local.is_const {
                return Ok((local.slot, true));
            }
            return Err(SourceLoweringError::unsupported("duplicate_binding", span));
        }
        self.allocate_local_with_mode(name, false, false, true, span)
            .map(|slot| (slot, false))
    }

    fn allocate_hoisted_local(
        &mut self,
        name: &'a str,
        is_const: bool,
        span: Span,
    ) -> Result<u16, SourceLoweringError> {
        self.allocate_local_with_mode(name, is_const, true, false, span)
    }

    fn allocate_initialized_local(
        &mut self,
        name: &'a str,
        is_const: bool,
        span: Span,
    ) -> Result<u16, SourceLoweringError> {
        let slot = self.allocate_local_with_mode(name, is_const, false, false, span)?;
        let local = self
            .locals
            .last_mut()
            .ok_or_else(|| SourceLoweringError::Internal("missing initialized local".into()))?;
        local.initialized = true;
        Ok(slot)
    }

    fn allocate_local_with_mode(
        &mut self,
        name: &'a str,
        is_const: bool,
        runtime_tdz: bool,
        is_var: bool,
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
            is_var,
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
            is_var: false,
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
                return Some(BindingRef::Upvalue {
                    idx,
                    is_const: entry.is_const,
                });
            }
        }
        None
    }

    fn resolve_capture(&self, name: &str) -> Option<BindingRef> {
        let parent = self.parent?;
        // Probe parent's own scope first.
        let resolved = match parent.resolve_own(name) {
            Some(BindingRef::Local { reg, is_const, .. }) => Some((
                crate::closure::CaptureDescriptor::Register(
                    crate::bytecode::BytecodeRegister::new(reg),
                ),
                is_const,
            )),
            Some(BindingRef::Param { reg }) => Some((
                crate::closure::CaptureDescriptor::Register(
                    crate::bytecode::BytecodeRegister::new(reg),
                ),
                false,
            )),
            Some(BindingRef::Upvalue { idx, is_const }) => Some((
                crate::closure::CaptureDescriptor::Upvalue(crate::closure::UpvalueId(idx)),
                is_const,
            )),
            None => None,
        };
        if let Some((descriptor, is_const)) = resolved {
            return Some(self.record_capture(name, descriptor, is_const));
        }
        // Parent didn't have it directly — recurse into parent's
        // parent. Parent grows its own captures list as part of
        // the recursive resolution, giving us a `parent_idx` to
        // chain through.
        let Some(BindingRef::Upvalue {
            idx: parent_idx,
            is_const,
        }) = parent.resolve_capture(name)
        else {
            return None;
        };
        let desc =
            crate::closure::CaptureDescriptor::Upvalue(crate::closure::UpvalueId(parent_idx));
        Some(self.record_capture(name, desc, is_const))
    }

    fn record_capture(
        &self,
        name: &str,
        descriptor: crate::closure::CaptureDescriptor,
        is_const: bool,
    ) -> BindingRef {
        let mut captures = self.captures.borrow_mut();
        let idx = u16::try_from(captures.len()).expect("capture count fits in u16");
        captures.push(CaptureEntry {
            name: name.to_owned(),
            descriptor,
            is_const,
        });
        BindingRef::Upvalue { idx, is_const }
    }

    fn take_captures(&self) -> Vec<crate::closure::CaptureDescriptor> {
        std::mem::take(&mut *self.captures.borrow_mut())
            .into_iter()
            .map(|entry| entry.descriptor)
            .collect()
    }
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
