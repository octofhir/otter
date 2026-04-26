//! AST → bytecode lowering with full foundation TS erasure.
//!
//! The compiler walks the OXC AST produced by `otter-syntax` and
//! emits an [`otter_bytecode::BytecodeModule`]. After task 08 the
//! frontend handles the **complete** foundation TypeScript subset
//! per [ADR-0002 §4](
//!     ../../../docs/new-engine/adr/0002-oxc-frontend.md
//!   ):
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
//! - [`compile`] — entry point.
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
//! # See also
//! - [`docs/new-engine/adr/0002-oxc-frontend.md`](
//!     ../../../docs/new-engine/adr/0002-oxc-frontend.md
//!   )

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use otter_bytecode::{
    BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind as BytecodeSourceKind,
    SpanEntry,
};
use otter_syntax::{Parsed, SourceKind as SyntaxSourceKind};
use oxc_ast::ast::{
    AssignmentOperator, AssignmentTarget, BinaryOperator, Expression, LogicalOperator, Statement,
    UnaryOperator,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Compile a parsed program into a [`BytecodeModule`].
///
/// `module_specifier` is recorded on the resulting bytecode and
/// surfaces in dump output, traces, and diagnostics.
///
/// # Errors
/// Returns [`CompileError`] when the AST contains constructs outside
/// the foundation subset (see [`CompileError::Unsupported`]).
pub fn compile(parsed: &Parsed, module_specifier: &str) -> Result<BytecodeModule, CompileError> {
    let program = parsed.program().map_err(|e| CompileError::Syntax {
        messages: e.messages,
    })?;

    let module = Rc::new(RefCell::new(ModuleBuilder::default()));
    // Reserve slot 0 for `<main>` so nested function compilation
    // can pre-register their ids deterministically (slice 13 only
    // needs the immediate id, but the slot reservation keeps the
    // table densely populated).
    module.borrow_mut().functions.push(Function {
        id: 0,
        name: "<main>".to_string(),
        span: (program.span.start, program.span.end),
        ..Default::default()
    });
    let mut cx = FunctionContext::new(Rc::clone(&module));
    cx.enter_scope();
    let mut last_value_reg: Option<u16> = None;

    for stmt in &program.body {
        if let Some(reg) = compile_statement(&mut cx, stmt)? {
            last_value_reg = Some(reg);
        }
    }
    cx.exit_scope();

    // Synthesize the program's completion value. If the body
    // produced one, return it; otherwise materialize `undefined` in
    // r0 and return that.
    let return_reg = match last_value_reg {
        Some(reg) => reg,
        None => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadUndefined,
                vec![Operand::Register(dst)],
                (program.span.start, program.span.end),
            );
            dst
        }
    };
    let span = (program.span.start, program.span.end);
    cx.emit(Op::Return, vec![Operand::Register(return_reg)], span);

    // Finalize `<main>` into the module's function table, then
    // drop `cx` so the module Rc has a single owner before
    // `try_unwrap`.
    {
        let mut m = module.borrow_mut();
        m.functions[0].locals = 0;
        m.functions[0].scratch = cx.scratch;
        m.functions[0].code = std::mem::take(&mut cx.code);
        m.functions[0].spans = std::mem::take(&mut cx.spans);
    }
    drop(cx);

    let kind = match parsed.kind {
        SyntaxSourceKind::JavaScript => BytecodeSourceKind::JavaScript,
        SyntaxSourceKind::TypeScript => BytecodeSourceKind::TypeScript,
    };

    let ModuleBuilder {
        functions,
        constants,
    } = Rc::try_unwrap(module)
        .expect("module builder should be uniquely owned at finalize")
        .into_inner();

    Ok(BytecodeModule {
        module: module_specifier.to_string(),
        source_kind: kind,
        functions,
        constants,
    })
}

/// Module-level mutable state shared across nested function
/// compilations. Threaded as `Rc<RefCell<ModuleBuilder>>` so the
/// `<main>` context and any nested function context can intern
/// constants into the same pool and register their `Function`
/// records into the same table without contorting the borrow
/// checker.
#[derive(Debug, Default)]
struct ModuleBuilder {
    functions: Vec<Function>,
    constants: Vec<Constant>,
}

/// One lexical scope's binding table. The compiler keeps a stack
/// of these so block-scoped `let`/`const` shadow correctly.
#[derive(Debug, Default)]
struct Scope {
    /// Map from binding name → register index (locals + scratch
    /// share one window in the foundation slice; locals occupy the
    /// low end).
    bindings: HashMap<String, BindingInfo>,
}

#[derive(Debug, Clone, Copy)]
struct BindingInfo {
    /// Register holding the binding's value.
    reg: u16,
    /// `true` for `const` declarations.
    is_const: bool,
    /// Whether the binding has been definitely initialized at the
    /// current compile point. `let x;` and `let x = init` start at
    /// `false` and flip to `true` after the initializer's
    /// `StoreLocal`. Reads before that emit `Op::TdzError`.
    initialized: bool,
}

/// One pending loop label so `break` / `continue` can patch their
/// offsets at scope close.
#[derive(Debug, Default)]
struct LoopFrame {
    /// Instruction PCs where `continue` emitted a placeholder
    /// JUMP. Patched to point at the loop's continue target (the
    /// update / test).
    continue_patches: Vec<u32>,
    /// Instruction PCs where `break` emitted a placeholder JUMP.
    /// Patched to point at the instruction after the loop body.
    break_patches: Vec<u32>,
}

/// Per-function compilation context.
#[derive(Debug)]
struct FunctionContext {
    module: Rc<RefCell<ModuleBuilder>>,
    code: Vec<Instruction>,
    spans: Vec<SpanEntry>,
    next_pc: u32,
    scratch: u16,
    /// Stack of lexical scopes. Index 0 is the function-body
    /// scope.
    scopes: Vec<Scope>,
    /// Stack of enclosing loops; the innermost is on top.
    loops: Vec<LoopFrame>,
}

impl FunctionContext {
    fn new(module: Rc<RefCell<ModuleBuilder>>) -> Self {
        Self {
            module,
            code: Vec::new(),
            spans: Vec::new(),
            next_pc: 0,
            scratch: 0,
            scopes: Vec::new(),
            loops: Vec::new(),
        }
    }

    fn alloc_scratch(&mut self) -> u16 {
        let r = self.scratch;
        self.scratch = self.scratch.checked_add(1).expect("register overflow");
        r
    }

    fn enter_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare_binding(
        &mut self,
        name: &str,
        is_const: bool,
        span: (u32, u32),
    ) -> Result<u16, CompileError> {
        let scope = self
            .scopes
            .last_mut()
            .expect("declare_binding called outside any scope");
        if scope.bindings.contains_key(name) {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
        let reg = self.scratch;
        self.scratch = self.scratch.checked_add(1).expect("register overflow");
        scope.bindings.insert(
            name.to_string(),
            BindingInfo {
                reg,
                is_const,
                initialized: false,
            },
        );
        Ok(reg)
    }

    fn lookup_binding(&self, name: &str) -> Option<BindingInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.bindings.get(name) {
                return Some(*info);
            }
        }
        None
    }

    /// Flip a binding's `initialized` flag to `true` once we've
    /// emitted its initializer's store. The compiler is intentionally
    /// conservative: we never flip back to `false` and we never
    /// "merge" branch states — task 14 ships the simple definite-
    /// assignment rule and leaves branch-aware refinement for a
    /// future slice.
    fn mark_initialized(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.bindings.get_mut(name) {
                info.initialized = true;
                return;
            }
        }
    }

    /// Emit a placeholder branch and return its instruction index
    /// so a later [`Self::patch_branch`] can fill in the offset.
    fn emit_branch_placeholder(&mut self, op: Op, cond_reg: Option<u16>, span: (u32, u32)) -> u32 {
        let mut operands: Vec<Operand> = Vec::with_capacity(2);
        operands.push(Operand::Imm32(0));
        if let Some(reg) = cond_reg {
            operands.push(Operand::Register(reg));
        }
        let pc = self.next_pc;
        self.code.push(Instruction { pc, op, operands });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
        pc
    }

    /// Patch a previously emitted branch so it targets the
    /// **current** `next_pc`.
    fn patch_branch_to_here(&mut self, branch_pc: u32) {
        let target = self.next_pc;
        self.patch_branch(branch_pc, target);
    }

    /// Patch a previously emitted branch to point at `target_pc`.
    fn patch_branch(&mut self, branch_pc: u32, target_pc: u32) {
        let offset = target_pc as i64 - (branch_pc as i64 + 1);
        let offset = i32::try_from(offset).expect("branch offset out of i32 range");
        let instr = self
            .code
            .iter_mut()
            .find(|i| i.pc == branch_pc)
            .expect("patch target missing");
        if let Some(Operand::Imm32(slot)) = instr.operands.first_mut() {
            *slot = offset;
        } else {
            panic!("patch target operand not Imm32");
        }
    }

    fn intern_string_constant(&mut self, value: &str) -> u32 {
        let utf16: Vec<u16> = value.encode_utf16().collect();
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::String { utf16: existing } = c
                && existing == &utf16
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::String { utf16 });
        (module.constants.len() - 1) as u32
    }

    fn intern_number_constant(&mut self, value: f64) -> u32 {
        let bits = value.to_bits();
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::Number { bits: existing } = c
                && *existing == bits
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::Number { bits });
        (module.constants.len() - 1) as u32
    }

    fn intern_function_id(&mut self, function_id: u32) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::FunctionId { index } = c
                && *index == function_id
            {
                return i as u32;
            }
        }
        module
            .constants
            .push(Constant::FunctionId { index: function_id });
        (module.constants.len() - 1) as u32
    }

    fn emit(&mut self, op: Op, operands: Vec<Operand>, span: (u32, u32)) {
        let pc = self.next_pc;
        self.code.push(Instruction { pc, op, operands });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
    }
}

/// Compile one statement. Returns `Some(reg)` when the statement is
/// an `ExpressionStatement` whose value should propagate as the
/// program's completion value; `None` otherwise.
fn compile_statement(
    cx: &mut FunctionContext,
    stmt: &Statement<'_>,
) -> Result<Option<u16>, CompileError> {
    if is_erased_ts_statement(stmt) {
        return Ok(None);
    }
    if let Some((node, span)) = rejected_ts_statement(stmt) {
        return Err(CompileError::TypeScriptUnsupported {
            node: node.to_string(),
            span,
        });
    }
    match stmt {
        Statement::EmptyStatement(_) => Ok(None),

        Statement::ExpressionStatement(es) => {
            let span = (es.span.start, es.span.end);
            let reg = compile_expr(cx, &es.expression, span)?;
            Ok(Some(reg))
        }

        Statement::BlockStatement(b) => {
            let span = (b.span.start, b.span.end);
            cx.enter_scope();
            let mut last = None;
            for inner in &b.body {
                if let Some(r) = compile_statement(cx, inner)? {
                    last = Some(r);
                }
            }
            cx.exit_scope();
            let _ = span;
            Ok(last)
        }

        Statement::VariableDeclaration(decl) => {
            let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
            let is_var = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
            if is_var {
                return Err(CompileError::Unsupported {
                    node: "VariableDeclaration (var; foundation rejects var)".to_string(),
                    span: (decl.span.start, decl.span.end),
                });
            }
            for declarator in &decl.declarations {
                let span = (declarator.span.start, declarator.span.end);
                let name = match &declarator.id {
                    oxc_ast::ast::BindingPattern::BindingIdentifier(id) => {
                        id.name.as_str().to_string()
                    }
                    _ => {
                        return Err(CompileError::Unsupported {
                            node: "VariableDeclarator pattern (non-identifier)".to_string(),
                            span,
                        });
                    }
                };
                let reg = cx.declare_binding(&name, is_const, span)?;
                // The initializer expression sees the binding in
                // its TDZ: `let x = x + 1;` should throw because
                // the right-hand `x` reads before initialization.
                let init_reg = match &declarator.init {
                    Some(init) => compile_expr(cx, init, span)?,
                    None => {
                        // No initializer → spec rule:
                        // `let x;` completes with `x = undefined`.
                        let dst = cx.alloc_scratch();
                        cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
                        dst
                    }
                };
                cx.emit(
                    Op::StoreLocal,
                    vec![Operand::Register(init_reg), Operand::Imm32(reg as i32)],
                    span,
                );
                cx.mark_initialized(&name);
            }
            Ok(None)
        }

        Statement::IfStatement(s) => {
            let span = (s.span.start, s.span.end);
            let cond_reg = compile_expr(cx, &s.test, span)?;
            // JUMP_IF_FALSE → after consequent
            let jmp_if_false = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
            compile_statement(cx, &s.consequent)?;
            if let Some(alt) = &s.alternate {
                // After consequent, unconditional JUMP past the
                // alternate.
                let jmp_end = cx.emit_branch_placeholder(Op::Jump, None, span);
                cx.patch_branch_to_here(jmp_if_false);
                compile_statement(cx, alt)?;
                cx.patch_branch_to_here(jmp_end);
            } else {
                cx.patch_branch_to_here(jmp_if_false);
            }
            Ok(None)
        }

        Statement::WhileStatement(s) => {
            let span = (s.span.start, s.span.end);
            let loop_top = cx.next_pc;
            cx.loops.push(LoopFrame::default());
            let cond_reg = compile_expr(cx, &s.test, span)?;
            let exit_jmp = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span);
            compile_statement(cx, &s.body)?;
            // Back-edge jump to loop top.
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, loop_top);
            cx.patch_branch_to_here(exit_jmp);
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, loop_top);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            Ok(None)
        }

        Statement::DoWhileStatement(s) => {
            let span = (s.span.start, s.span.end);
            let body_top = cx.next_pc;
            cx.loops.push(LoopFrame::default());
            compile_statement(cx, &s.body)?;
            let continue_target = cx.next_pc;
            let cond_reg = compile_expr(cx, &s.test, span)?;
            let back_jmp = cx.emit_branch_placeholder(Op::JumpIfTrue, Some(cond_reg), span);
            cx.patch_branch(back_jmp, body_top);
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, continue_target);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            Ok(None)
        }

        Statement::ForStatement(s) => {
            let span = (s.span.start, s.span.end);
            cx.enter_scope();
            // Initializer.
            if let Some(init) = &s.init {
                match init {
                    oxc_ast::ast::ForStatementInit::VariableDeclaration(decl) => {
                        compile_for_init_decl(cx, decl, span)?;
                    }
                    other => {
                        if let Some(expr) = init_to_expression(other) {
                            compile_expr(cx, expr, span)?;
                        }
                    }
                }
            }
            cx.loops.push(LoopFrame::default());
            let test_top = cx.next_pc;
            // Test.
            let exit_patch = if let Some(test) = &s.test {
                let cond_reg = compile_expr(cx, test, span)?;
                Some(cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond_reg), span))
            } else {
                None
            };
            // Body.
            compile_statement(cx, &s.body)?;
            // Continue lands on the update.
            let update_pc = cx.next_pc;
            if let Some(update) = &s.update {
                compile_expr(cx, update, span)?;
            }
            let back_jmp = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch(back_jmp, test_top);
            if let Some(p) = exit_patch {
                cx.patch_branch_to_here(p);
            }
            let frame = cx.loops.pop().expect("loop frame disappeared");
            for pc in frame.continue_patches {
                cx.patch_branch(pc, update_pc);
            }
            for pc in frame.break_patches {
                cx.patch_branch_to_here(pc);
            }
            cx.exit_scope();
            Ok(None)
        }

        Statement::BreakStatement(s) => {
            let span = (s.span.start, s.span.end);
            if s.label.is_some() {
                return Err(CompileError::Unsupported {
                    node: "BreakStatement (labeled)".to_string(),
                    span,
                });
            }
            let loop_idx = cx
                .loops
                .len()
                .checked_sub(1)
                .ok_or(CompileError::Unsupported {
                    node: "BreakStatement outside any loop".to_string(),
                    span,
                })?;
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[loop_idx].break_patches.push(pc);
            Ok(None)
        }

        Statement::FunctionDeclaration(f) => {
            let span = (f.span.start, f.span.end);
            let name =
                f.id.as_ref()
                    .ok_or(CompileError::Unsupported {
                        node: "FunctionDeclaration without name".to_string(),
                        span,
                    })?
                    .name
                    .as_str()
                    .to_string();
            let function_id = compile_function(cx, &name, &f.params, &f.body, span)?;
            // Bind the name in the current scope to a register
            // holding the function value. Foundation slice doesn't
            // hoist; declarations are evaluated at their lexical
            // position.
            let reg = cx.declare_binding(&name, false, span)?;
            let const_idx = cx.intern_function_id(function_id);
            let tmp = cx.alloc_scratch();
            cx.emit(
                Op::MakeFunction,
                vec![Operand::Register(tmp), Operand::ConstIndex(const_idx)],
                span,
            );
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(tmp), Operand::Imm32(reg as i32)],
                span,
            );
            cx.mark_initialized(&name);
            Ok(None)
        }

        Statement::ReturnStatement(r) => {
            let span = (r.span.start, r.span.end);
            match &r.argument {
                Some(arg) => {
                    let reg = compile_expr(cx, arg, span)?;
                    cx.emit(Op::ReturnValue, vec![Operand::Register(reg)], span);
                }
                None => {
                    cx.emit(Op::ReturnUndefined, vec![], span);
                }
            }
            Ok(None)
        }

        Statement::ContinueStatement(s) => {
            let span = (s.span.start, s.span.end);
            if s.label.is_some() {
                return Err(CompileError::Unsupported {
                    node: "ContinueStatement (labeled)".to_string(),
                    span,
                });
            }
            let loop_idx = cx
                .loops
                .len()
                .checked_sub(1)
                .ok_or(CompileError::Unsupported {
                    node: "ContinueStatement outside any loop".to_string(),
                    span,
                })?;
            let pc = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.loops[loop_idx].continue_patches.push(pc);
            Ok(None)
        }

        other => Err(CompileError::Unsupported {
            node: stmt_kind_name(other).to_string(),
            span: stmt_span(other),
        }),
    }
}

/// Helper for the `for(...; ...; ...)` initializer's
/// `let`/`const`/`var` declaration form. Mirrors the
/// `VariableDeclaration` arm of `compile_statement` but operates on
/// the borrowed declaration without re-cloning it through OXC's
/// allocator.
fn compile_for_init_decl(
    cx: &mut FunctionContext,
    decl: &oxc_ast::ast::VariableDeclaration<'_>,
    span: (u32, u32),
) -> Result<(), CompileError> {
    let is_const = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Const);
    let is_var = matches!(decl.kind, oxc_ast::ast::VariableDeclarationKind::Var);
    if is_var {
        return Err(CompileError::Unsupported {
            node: "for-init `var` (foundation rejects var)".to_string(),
            span,
        });
    }
    for declarator in &decl.declarations {
        let span = (declarator.span.start, declarator.span.end);
        let name = match &declarator.id {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.name.as_str().to_string(),
            _ => {
                return Err(CompileError::Unsupported {
                    node: "for-init declarator pattern (non-identifier)".to_string(),
                    span,
                });
            }
        };
        let reg = cx.declare_binding(&name, is_const, span)?;
        let init_reg = match &declarator.init {
            Some(init) => compile_expr(cx, init, span)?,
            None => {
                let dst = cx.alloc_scratch();
                cx.emit(Op::LoadUndefined, vec![Operand::Register(dst)], span);
                dst
            }
        };
        cx.emit(
            Op::StoreLocal,
            vec![Operand::Register(init_reg), Operand::Imm32(reg as i32)],
            span,
        );
        cx.mark_initialized(&name);
    }
    Ok(())
}

/// Compile a function body into a fresh `Function` record and
/// return its id. Parameters are bound as locals at registers
/// `0..param_count`. Foundation subset rejects rest / default /
/// destructuring parameters.
fn compile_function(
    parent: &mut FunctionContext,
    name: &str,
    params: &oxc_ast::ast::FormalParameters<'_>,
    body: &Option<oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody<'_>>>,
    span: (u32, u32),
) -> Result<u32, CompileError> {
    if params.rest.is_some() {
        return Err(CompileError::Unsupported {
            node: "FunctionDeclaration: rest parameter".to_string(),
            span,
        });
    }
    let mut cx = FunctionContext::new(Rc::clone(&parent.module));
    cx.enter_scope();
    let mut param_count: u16 = 0;
    for param in &params.items {
        if param.pattern.kind_initializer().is_some() {
            return Err(CompileError::Unsupported {
                node: "FunctionDeclaration: default parameter".to_string(),
                span,
            });
        }
        let name = match &param.pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.name.as_str().to_string(),
            _ => {
                return Err(CompileError::Unsupported {
                    node: "FunctionDeclaration: destructuring parameter".to_string(),
                    span,
                });
            }
        };
        // Parameter binding lives at register index = param ordinal
        // and is initialized by the caller's argument-binding step,
        // so the body can read it without TDZ.
        cx.declare_binding(&name, false, span)?;
        cx.mark_initialized(&name);
        param_count = param_count.checked_add(1).expect("too many parameters");
    }

    // Reserve the function's id ahead of compilation so the body
    // can reference its own name (recursion). This mirrors the
    // "function declaration is bound in its own scope" semantics
    // without needing a real closure model — slice 13 keeps
    // upvalues out.
    let function_id = parent.module.borrow().functions.len() as u32;
    parent.module.borrow_mut().functions.push(Function {
        id: function_id,
        name: name.to_string(),
        span,
        ..Default::default()
    });
    let self_reg = cx.declare_binding(name, false, span)?;
    let const_idx = cx.intern_function_id(function_id);
    let tmp = cx.alloc_scratch();
    cx.emit(
        Op::MakeFunction,
        vec![Operand::Register(tmp), Operand::ConstIndex(const_idx)],
        span,
    );
    cx.emit(
        Op::StoreLocal,
        vec![Operand::Register(tmp), Operand::Imm32(self_reg as i32)],
        span,
    );
    cx.mark_initialized(name);

    if let Some(body) = body {
        for stmt in &body.statements {
            compile_statement(&mut cx, stmt)?;
        }
    }
    cx.exit_scope();
    // Implicit `return undefined;` at the function tail.
    cx.emit(Op::ReturnUndefined, vec![], span);

    let mut module = parent.module.borrow_mut();
    let slot = module
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = cx.scratch;
    slot.param_count = param_count;
    slot.code = cx.code;
    slot.spans = cx.spans;
    Ok(function_id)
}

/// Compile an arrow function. Two body shapes share the same
/// lowering:
///
/// - `() => expr` (expression body): one synthetic
///   `ReturnValue(expr)`.
/// - `() => { ... }` (block body): existing function-body
///   compilation, with an implicit `ReturnUndefined` tail.
///
/// Captured-environment access (`this`, outer-scope variables) is
/// **not** supported in this slice — the compiler creates a fresh
/// `FunctionContext` so a body that references an outer-scope
/// binding fails fast with a clear `unresolved identifier`
/// diagnostic.
fn compile_arrow_function(
    parent: &mut FunctionContext,
    arrow: &oxc_ast::ast::ArrowFunctionExpression<'_>,
    span: (u32, u32),
) -> Result<u32, CompileError> {
    if arrow.params.rest.is_some() {
        return Err(CompileError::Unsupported {
            node: "ArrowFunction: rest parameter".to_string(),
            span,
        });
    }
    if arrow.r#async {
        return Err(CompileError::Unsupported {
            node: "ArrowFunction: async".to_string(),
            span,
        });
    }
    let mut cx = FunctionContext::new(Rc::clone(&parent.module));
    cx.enter_scope();

    let mut param_count: u16 = 0;
    for param in &arrow.params.items {
        if param.pattern.kind_initializer().is_some() {
            return Err(CompileError::Unsupported {
                node: "ArrowFunction: default parameter".to_string(),
                span,
            });
        }
        let name = match &param.pattern {
            oxc_ast::ast::BindingPattern::BindingIdentifier(id) => id.name.as_str().to_string(),
            _ => {
                return Err(CompileError::Unsupported {
                    node: "ArrowFunction: destructuring parameter".to_string(),
                    span,
                });
            }
        };
        cx.declare_binding(&name, false, span)?;
        cx.mark_initialized(&name);
        param_count = param_count.checked_add(1).expect("too many parameters");
    }

    // Reserve the function record up front so we can emit
    // `MakeFunction` for the result later.
    let function_id = parent.module.borrow().functions.len() as u32;
    parent.module.borrow_mut().functions.push(Function {
        id: function_id,
        name: "<arrow>".to_string(),
        span,
        ..Default::default()
    });

    if arrow.expression {
        // `() => expr` — body is a single ExpressionStatement
        // whose expression is the implicit return value.
        let stmt = arrow
            .body
            .statements
            .first()
            .ok_or(CompileError::Unsupported {
                node: "ArrowFunction: empty expression body".to_string(),
                span,
            })?;
        let Statement::ExpressionStatement(es) = stmt else {
            return Err(CompileError::Unsupported {
                node: "ArrowFunction: malformed expression body".to_string(),
                span,
            });
        };
        let inner_span = (es.span.start, es.span.end);
        let reg = compile_expr(&mut cx, &es.expression, inner_span)?;
        cx.emit(Op::ReturnValue, vec![Operand::Register(reg)], inner_span);
    } else {
        for stmt in &arrow.body.statements {
            compile_statement(&mut cx, stmt)?;
        }
        cx.emit(Op::ReturnUndefined, vec![], span);
    }
    cx.exit_scope();

    let mut module = parent.module.borrow_mut();
    let slot = module
        .functions
        .get_mut(function_id as usize)
        .expect("reserved function slot");
    slot.locals = 0;
    slot.scratch = cx.scratch;
    slot.param_count = param_count;
    slot.code = cx.code;
    slot.spans = cx.spans;
    Ok(function_id)
}

/// Tiny helper: detect whether a `BindingPattern` carries a
/// default-value initializer. OXC's `BindingPattern` is a flat enum
/// which doesn't directly expose the initializer; the foundation
/// subset uses this only as a guard so we can return a clean
/// diagnostic instead of silently dropping the default.
trait BindingPatternExt {
    fn kind_initializer(&self) -> Option<()>;
}

impl BindingPatternExt for oxc_ast::ast::BindingPattern<'_> {
    fn kind_initializer(&self) -> Option<()> {
        match self {
            oxc_ast::ast::BindingPattern::AssignmentPattern(_) => Some(()),
            _ => None,
        }
    }
}

/// Adapter for the `for(...; ...; ...)` initializer's
/// `Expression`-shaped variant. OXC's `ForStatementInit` is a
/// closed enum that mirrors `Expression`; this helper widens it
/// back to `&Expression` so the compiler can reuse `compile_expr`.
fn init_to_expression<'a, 'b>(
    init: &'a oxc_ast::ast::ForStatementInit<'b>,
) -> Option<&'a Expression<'b>> {
    init.as_expression()
}

fn compile_expr(
    cx: &mut FunctionContext,
    expr: &Expression<'_>,
    enclosing_span: (u32, u32),
) -> Result<u16, CompileError> {
    let expr = unwrap_ts_expr(expr);
    match expr {
        Expression::Identifier(id) if id.name.as_str() == "undefined" => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadUndefined,
                vec![Operand::Register(dst)],
                enclosing_span,
            );
            Ok(dst)
        }

        Expression::NullLiteral(lit) => {
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadNull,
                vec![Operand::Register(dst)],
                (lit.span.start, lit.span.end),
            );
            Ok(dst)
        }

        Expression::Identifier(id) => {
            let span = (id.span.start, id.span.end);
            // Foundation pseudo-globals before falling back to
            // local resolution.
            match id.name.as_str() {
                "NaN" => {
                    let dst = cx.alloc_scratch();
                    let const_idx = cx.intern_number_constant(f64::NAN);
                    cx.emit(
                        Op::LoadNumber,
                        vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                        span,
                    );
                    return Ok(dst);
                }
                "Infinity" => {
                    let dst = cx.alloc_scratch();
                    let const_idx = cx.intern_number_constant(f64::INFINITY);
                    cx.emit(
                        Op::LoadNumber,
                        vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                        span,
                    );
                    return Ok(dst);
                }
                _ => {}
            }
            match cx.lookup_binding(id.name.as_str()) {
                Some(info) => {
                    let dst = cx.alloc_scratch();
                    if info.initialized {
                        cx.emit(
                            Op::LoadLocal,
                            vec![Operand::Register(dst), Operand::Imm32(info.reg as i32)],
                            span,
                        );
                    } else {
                        // Reading a `let` / `const` binding before
                        // its initializer ran — the runtime will
                        // raise a `ReferenceError`-equivalent
                        // diagnostic via `Op::TdzError`.
                        cx.emit(Op::TdzError, vec![Operand::Imm32(info.reg as i32)], span);
                    }
                    Ok(dst)
                }
                None => Err(CompileError::Unsupported {
                    node: format!("unresolved identifier `{}`", id.name),
                    span,
                }),
            }
        }

        Expression::LogicalExpression(l) => {
            let span = (l.span.start, l.span.end);
            // Lower `a && b`, `a || b`, `a ?? b` with short-circuit
            // semantics. The result lands in a fresh register and
            // both branches store into the same slot.
            let left = compile_expr(cx, &l.left, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(left), Operand::Imm32(dst as i32)],
                span,
            );
            // Note: locals and scratch share the same register
            // window. We use STORE_LOCAL into the freshly-allocated
            // scratch index so the JUMP target reads back through
            // LOAD_LOCAL — preserves register liveness across the
            // branch without a phi.
            let short_circuit = match l.operator {
                LogicalOperator::And => {
                    cx.emit_branch_placeholder(Op::JumpIfFalse, Some(left), span)
                }
                LogicalOperator::Or => cx.emit_branch_placeholder(Op::JumpIfTrue, Some(left), span),
                LogicalOperator::Coalesce => {
                    // `a ?? b`: if `a` is **not** nullish, short-
                    // circuit. JumpIfNullish jumps when nullish, so
                    // we want the **inverse**: emit a normal branch
                    // into "evaluate b" path when nullish, and let
                    // fall-through skip past `b`. Implement via two
                    // jumps for clarity.
                    let to_b = cx.emit_branch_placeholder(Op::JumpIfNullish, Some(left), span);
                    let skip = cx.emit_branch_placeholder(Op::Jump, None, span);
                    cx.patch_branch_to_here(to_b);
                    let right = compile_expr(cx, &l.right, span)?;
                    cx.emit(
                        Op::StoreLocal,
                        vec![Operand::Register(right), Operand::Imm32(dst as i32)],
                        span,
                    );
                    cx.patch_branch_to_here(skip);
                    return Ok({
                        let out = cx.alloc_scratch();
                        cx.emit(
                            Op::LoadLocal,
                            vec![Operand::Register(out), Operand::Imm32(dst as i32)],
                            span,
                        );
                        out
                    });
                }
            };
            // Falling here for `&&` / `||`: evaluate `right` and
            // store; patch short-circuit at end.
            let right = compile_expr(cx, &l.right, span)?;
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(right), Operand::Imm32(dst as i32)],
                span,
            );
            cx.patch_branch_to_here(short_circuit);
            let out = cx.alloc_scratch();
            cx.emit(
                Op::LoadLocal,
                vec![Operand::Register(out), Operand::Imm32(dst as i32)],
                span,
            );
            Ok(out)
        }

        Expression::ConditionalExpression(c) => {
            let span = (c.span.start, c.span.end);
            let cond = compile_expr(cx, &c.test, span)?;
            let dst = cx.alloc_scratch();
            let to_alt = cx.emit_branch_placeholder(Op::JumpIfFalse, Some(cond), span);
            let cons = compile_expr(cx, &c.consequent, span)?;
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(cons), Operand::Imm32(dst as i32)],
                span,
            );
            let to_end = cx.emit_branch_placeholder(Op::Jump, None, span);
            cx.patch_branch_to_here(to_alt);
            let alt = compile_expr(cx, &c.alternate, span)?;
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(alt), Operand::Imm32(dst as i32)],
                span,
            );
            cx.patch_branch_to_here(to_end);
            let out = cx.alloc_scratch();
            cx.emit(
                Op::LoadLocal,
                vec![Operand::Register(out), Operand::Imm32(dst as i32)],
                span,
            );
            Ok(out)
        }

        Expression::AssignmentExpression(a) => {
            let span = (a.span.start, a.span.end);
            if !matches!(a.operator, AssignmentOperator::Assign) {
                return Err(CompileError::Unsupported {
                    node: format!("AssignmentExpression ({:?})", a.operator),
                    span,
                });
            }
            // `obj.prop = value` — emit StoreProperty.
            if let AssignmentTarget::StaticMemberExpression(member) = &a.left {
                let obj_reg = compile_expr(cx, &member.object, span)?;
                let value = compile_expr(cx, &a.right, span)?;
                let name_idx = cx.intern_string_constant(member.property.name.as_str());
                cx.emit(
                    Op::StoreProperty,
                    vec![
                        Operand::Register(obj_reg),
                        Operand::ConstIndex(name_idx),
                        Operand::Register(value),
                    ],
                    span,
                );
                return Ok(value);
            }
            // `arr[i] = value` — emit StoreElement.
            if let AssignmentTarget::ComputedMemberExpression(member) = &a.left {
                let arr_reg = compile_expr(cx, &member.object, span)?;
                let idx_reg = compile_expr(cx, &member.expression, span)?;
                let value = compile_expr(cx, &a.right, span)?;
                cx.emit(
                    Op::StoreElement,
                    vec![
                        Operand::Register(arr_reg),
                        Operand::Register(idx_reg),
                        Operand::Register(value),
                    ],
                    span,
                );
                return Ok(value);
            }
            // `name = value` — local binding store.
            let name = match &a.left {
                AssignmentTarget::AssignmentTargetIdentifier(id) => id.name.as_str().to_string(),
                _ => {
                    return Err(CompileError::Unsupported {
                        node: "AssignmentTarget (non-identifier)".to_string(),
                        span,
                    });
                }
            };
            let info = cx.lookup_binding(&name).ok_or(CompileError::Unsupported {
                node: format!("assignment to undeclared `{name}`"),
                span,
            })?;
            if info.is_const {
                return Err(CompileError::Unsupported {
                    node: format!("assignment to const `{name}`"),
                    span,
                });
            }
            let value = compile_expr(cx, &a.right, span)?;
            cx.emit(
                Op::StoreLocal,
                vec![Operand::Register(value), Operand::Imm32(info.reg as i32)],
                span,
            );
            // An explicit assignment counts as initialization for
            // TDZ purposes, so subsequent reads stop emitting
            // `TdzError`. (Note: per spec, an `x = 1` that
            // *precedes* the `let x` declaration is itself a TDZ
            // ReferenceError; that case requires hoisting and is
            // tracked as a separate task.)
            cx.mark_initialized(&name);
            Ok(value)
        }

        Expression::StringLiteral(lit) => {
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_string_constant(&lit.value);
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                (lit.span.start, lit.span.end),
            );
            Ok(dst)
        }

        Expression::NumericLiteral(lit) => {
            let dst = cx.alloc_scratch();
            let span = (lit.span.start, lit.span.end);
            // Smi fast path: integer-valued literal in i32 range.
            if lit.value.fract() == 0.0
                && lit.value.is_finite()
                && (i32::MIN as f64..=i32::MAX as f64).contains(&lit.value)
                && !(lit.value == 0.0 && lit.value.is_sign_negative())
            {
                cx.emit(
                    Op::LoadInt32,
                    vec![Operand::Register(dst), Operand::Imm32(lit.value as i32)],
                    span,
                );
            } else {
                let const_idx = cx.intern_number_constant(lit.value);
                cx.emit(
                    Op::LoadNumber,
                    vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                    span,
                );
            }
            Ok(dst)
        }

        Expression::BooleanLiteral(lit) => {
            let dst = cx.alloc_scratch();
            let span = (lit.span.start, lit.span.end);
            cx.emit(
                if lit.value {
                    Op::LoadTrue
                } else {
                    Op::LoadFalse
                },
                vec![Operand::Register(dst)],
                span,
            );
            Ok(dst)
        }

        Expression::UnaryExpression(u) => {
            let span = (u.span.start, u.span.end);
            // `delete obj.prop` is special: the operand isn't a
            // value-producing expression, it's a member reference.
            if matches!(u.operator, UnaryOperator::Delete) {
                if let Expression::StaticMemberExpression(member) = &u.argument {
                    let obj_reg = compile_expr(cx, &member.object, span)?;
                    let name_idx = cx.intern_string_constant(member.property.name.as_str());
                    let dst = cx.alloc_scratch();
                    cx.emit(
                        Op::DeleteProperty,
                        vec![
                            Operand::Register(dst),
                            Operand::Register(obj_reg),
                            Operand::ConstIndex(name_idx),
                        ],
                        span,
                    );
                    return Ok(dst);
                }
                return Err(CompileError::Unsupported {
                    node: "delete on non-member expression".to_string(),
                    span,
                });
            }
            let inner = compile_expr(cx, &u.argument, span)?;
            let dst = cx.alloc_scratch();
            let op = match u.operator {
                UnaryOperator::UnaryNegation => Op::Neg,
                UnaryOperator::UnaryPlus => Op::ToNumber,
                UnaryOperator::LogicalNot => Op::LogicalNot,
                other => {
                    return Err(CompileError::Unsupported {
                        node: format!("UnaryExpression ({other:?})"),
                        span,
                    });
                }
            };
            cx.emit(
                op,
                vec![Operand::Register(dst), Operand::Register(inner)],
                span,
            );
            Ok(dst)
        }

        Expression::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => {
            let quasi = &t.quasis[0];
            let cooked = quasi.value.cooked.as_deref().unwrap_or("");
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_string_constant(cooked);
            cx.emit(
                Op::LoadString,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                (t.span.start, t.span.end),
            );
            Ok(dst)
        }

        Expression::BinaryExpression(b) => {
            let span = (b.span.start, b.span.end);
            let lhs = compile_expr(cx, &b.left, span)?;
            let rhs = compile_expr(cx, &b.right, span)?;
            let op = match b.operator {
                BinaryOperator::Addition => Op::Add,
                BinaryOperator::Subtraction => Op::Sub,
                BinaryOperator::Multiplication => Op::Mul,
                BinaryOperator::Division => Op::Div,
                BinaryOperator::Remainder => Op::Rem,
                BinaryOperator::StrictEquality => Op::Equal,
                BinaryOperator::StrictInequality => Op::NotEqual,
                BinaryOperator::LessThan => Op::LessThan,
                BinaryOperator::LessEqualThan => Op::LessEq,
                BinaryOperator::GreaterThan => Op::GreaterThan,
                BinaryOperator::GreaterEqualThan => Op::GreaterEq,
                BinaryOperator::Instanceof => Op::Instanceof,
                other => {
                    return Err(CompileError::Unsupported {
                        node: format!("BinaryExpression ({other:?})"),
                        span,
                    });
                }
            };
            let dst = cx.alloc_scratch();
            cx.emit(
                op,
                vec![
                    Operand::Register(dst),
                    Operand::Register(lhs),
                    Operand::Register(rhs),
                ],
                span,
            );
            Ok(dst)
        }

        Expression::StaticMemberExpression(m) => {
            // General named member access. The runtime resolves
            // `string.length` as the special-case length getter and
            // walks `JsObject` properties for objects.
            let span = (m.span.start, m.span.end);
            let receiver = compile_expr(cx, &m.object, span)?;
            let name_idx = cx.intern_string_constant(m.property.name.as_str());
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadProperty,
                vec![
                    Operand::Register(dst),
                    Operand::Register(receiver),
                    Operand::ConstIndex(name_idx),
                ],
                span,
            );
            Ok(dst)
        }

        // `s[i]` — runtime checks that `s` is a string.
        Expression::ComputedMemberExpression(m) => {
            let span = (m.span.start, m.span.end);
            let recv = compile_expr(cx, &m.object, span)?;
            let idx = compile_expr(cx, &m.expression, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadElement,
                vec![
                    Operand::Register(dst),
                    Operand::Register(recv),
                    Operand::Register(idx),
                ],
                span,
            );
            Ok(dst)
        }

        // `recv.method(arg0, arg1, ...)` — dispatched through the
        // String.prototype intrinsic table at run time.
        Expression::CallExpression(call) => compile_method_call(cx, call),

        Expression::ParenthesizedExpression(p) => {
            compile_expr(cx, &p.expression, (p.span.start, p.span.end))
        }

        Expression::ArrayExpression(arr) => {
            let span = (arr.span.start, arr.span.end);
            let mut element_regs: Vec<u16> = Vec::with_capacity(arr.elements.len());
            for el in &arr.elements {
                match el {
                    oxc_ast::ast::ArrayExpressionElement::SpreadElement(s) => {
                        return Err(CompileError::Unsupported {
                            node: "ArrayExpression: spread element".to_string(),
                            span: (s.span.start, s.span.end),
                        });
                    }
                    oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                        // Hole: foundation slice fills with `undefined`.
                        let r = cx.alloc_scratch();
                        cx.emit(Op::LoadUndefined, vec![Operand::Register(r)], span);
                        element_regs.push(r);
                    }
                    other => {
                        let expr = other.to_expression();
                        element_regs.push(compile_expr(cx, expr, span)?);
                    }
                }
            }
            let dst = cx.alloc_scratch();
            let mut operands: Vec<Operand> = Vec::with_capacity(2 + element_regs.len());
            operands.push(Operand::Register(dst));
            operands.push(Operand::ConstIndex(element_regs.len() as u32));
            operands.extend(element_regs.into_iter().map(Operand::Register));
            cx.emit(Op::NewArray, operands, span);
            Ok(dst)
        }

        Expression::ObjectExpression(obj) => {
            let span = (obj.span.start, obj.span.end);
            let dst = cx.alloc_scratch();
            cx.emit(Op::NewObject, vec![Operand::Register(dst)], span);
            for prop in &obj.properties {
                match prop {
                    oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) => {
                        let key_span = (p.span.start, p.span.end);
                        if p.computed {
                            return Err(CompileError::Unsupported {
                                node: "ObjectExpression: computed key".to_string(),
                                span: key_span,
                            });
                        }
                        if !matches!(p.kind, oxc_ast::ast::PropertyKind::Init) {
                            return Err(CompileError::Unsupported {
                                node: "ObjectExpression: getter/setter".to_string(),
                                span: key_span,
                            });
                        }
                        let key_str = match &p.key {
                            oxc_ast::ast::PropertyKey::StaticIdentifier(id) => {
                                id.name.as_str().to_string()
                            }
                            oxc_ast::ast::PropertyKey::StringLiteral(lit) => lit.value.to_string(),
                            _ => {
                                return Err(CompileError::Unsupported {
                                    node: "ObjectExpression: non-string property key".to_string(),
                                    span: key_span,
                                });
                            }
                        };
                        let value_reg = compile_expr(cx, &p.value, key_span)?;
                        let const_idx = cx.intern_string_constant(&key_str);
                        cx.emit(
                            Op::StoreProperty,
                            vec![
                                Operand::Register(dst),
                                Operand::ConstIndex(const_idx),
                                Operand::Register(value_reg),
                            ],
                            key_span,
                        );
                    }
                    oxc_ast::ast::ObjectPropertyKind::SpreadProperty(s) => {
                        return Err(CompileError::Unsupported {
                            node: "ObjectExpression: spread element".to_string(),
                            span: (s.span.start, s.span.end),
                        });
                    }
                }
            }
            Ok(dst)
        }

        Expression::FunctionExpression(f) => {
            let span = (f.span.start, f.span.end);
            let name =
                f.id.as_ref()
                    .map(|id| id.name.as_str().to_string())
                    .unwrap_or_else(|| "<anonymous>".to_string());
            let function_id = compile_function(cx, &name, &f.params, &f.body, span)?;
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            cx.emit(
                Op::MakeFunction,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                span,
            );
            Ok(dst)
        }

        Expression::ArrowFunctionExpression(a) => {
            let span = (a.span.start, a.span.end);
            let function_id = compile_arrow_function(cx, a, span)?;
            let dst = cx.alloc_scratch();
            let const_idx = cx.intern_function_id(function_id);
            cx.emit(
                Op::MakeFunction,
                vec![Operand::Register(dst), Operand::ConstIndex(const_idx)],
                span,
            );
            Ok(dst)
        }

        other => Err(CompileError::Unsupported {
            node: format!("Expression ({})", expr_kind_name(other)),
            span: expr_span(other),
        }),
    }
}

/// Lower a call expression. Two forms are supported:
///
/// - `receiver.method(args...)` — dispatched through the
///   `String.prototype` intrinsic table at run time.
/// - `callee(args...)` (free call) — emits `Op::Call`. Callee must
///   evaluate to a `Value::Function` at run time; otherwise the VM
///   raises `NotCallable`.
///
/// Computed-method access, `new`, and spread arguments are
/// deferred.
fn compile_method_call(
    cx: &mut FunctionContext,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (call.span.start, call.span.end);
    let callee = unwrap_ts_expr(&call.callee);
    if let Expression::StaticMemberExpression(member) = callee {
        // Foundation built-ins on the global `Object`: lower a few
        // canonical forms directly to dedicated opcodes so the
        // runtime does not need a host-callable bridge yet.
        if let Expression::Identifier(id) = &member.object
            && id.name.as_str() == "Object"
        {
            let method = member.property.name.as_str();
            let arg_regs = compile_call_args(cx, &call.arguments, span)?;
            return compile_object_builtin(cx, method, &arg_regs, span);
        }
        let receiver_reg = compile_expr(cx, &member.object, span)?;
        let name = member.property.name.as_str();
        let name_idx = cx.intern_string_constant(name);
        let arg_regs = compile_call_args(cx, &call.arguments, span)?;
        let dst = cx.alloc_scratch();
        let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
        operands.push(Operand::Register(dst));
        operands.push(Operand::Register(receiver_reg));
        operands.push(Operand::ConstIndex(name_idx));
        operands.push(Operand::ConstIndex(arg_regs.len() as u32));
        operands.extend(arg_regs.into_iter().map(Operand::Register));
        cx.emit(Op::CallStringMethod, operands, span);
        return Ok(dst);
    }
    // Free call: `callee(args...)`.
    let callee_reg = compile_expr(cx, callee, span)?;
    let arg_regs = compile_call_args(cx, &call.arguments, span)?;
    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(3 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(callee_reg));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::Call, operands, span);
    Ok(dst)
}

/// Lower a recognised `Object.<method>(args...)` call site to its
/// dedicated opcode. Foundation slice 19 covers `create`,
/// `getPrototypeOf`, and `setPrototypeOf`.
fn compile_object_builtin(
    cx: &mut FunctionContext,
    method: &str,
    arg_regs: &[u16],
    span: (u32, u32),
) -> Result<u16, CompileError> {
    match (method, arg_regs.len()) {
        ("create", 1) => {
            let proto_reg = arg_regs[0];
            let dst = cx.alloc_scratch();
            cx.emit(Op::NewObject, vec![Operand::Register(dst)], span);
            cx.emit(
                Op::SetPrototype,
                vec![Operand::Register(dst), Operand::Register(proto_reg)],
                span,
            );
            Ok(dst)
        }
        ("getPrototypeOf", 1) => {
            let obj_reg = arg_regs[0];
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::GetPrototype,
                vec![Operand::Register(dst), Operand::Register(obj_reg)],
                span,
            );
            Ok(dst)
        }
        ("setPrototypeOf", 2) => {
            let obj_reg = arg_regs[0];
            let proto_reg = arg_regs[1];
            cx.emit(
                Op::SetPrototype,
                vec![Operand::Register(obj_reg), Operand::Register(proto_reg)],
                span,
            );
            // Spec says `setPrototypeOf` returns `obj`; foundation
            // mirrors that.
            Ok(obj_reg)
        }
        _ => Err(CompileError::Unsupported {
            node: format!("Object.{method}/{}", arg_regs.len()),
            span,
        }),
    }
}

fn compile_call_args(
    cx: &mut FunctionContext,
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

fn expr_kind_name(expr: &Expression<'_>) -> &'static str {
    use Expression::*;
    match expr {
        Identifier(_) => "Identifier",
        StringLiteral(_) => "StringLiteral",
        NumericLiteral(_) => "NumericLiteral",
        BooleanLiteral(_) => "BooleanLiteral",
        NullLiteral(_) => "NullLiteral",
        TemplateLiteral(_) => "TemplateLiteral",
        BinaryExpression(_) => "BinaryExpression",
        StaticMemberExpression(_) => "StaticMemberExpression",
        CallExpression(_) => "CallExpression",
        FunctionExpression(_) => "FunctionExpression",
        ArrayExpression(_) => "ArrayExpression",
        ObjectExpression(_) => "ObjectExpression",
        ParenthesizedExpression(_) => "ParenthesizedExpression",
        _ => "Expression",
    }
}

fn expr_span(expr: &Expression<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = expr.span();
    (s.start, s.end)
}

/// Strip TypeScript-only expression wrappers and parentheses,
/// returning the underlying runtime expression.
///
/// Recognises `TSAsExpression`, `TSSatisfiesExpression`,
/// `TSNonNullExpression`, `TSTypeAssertion`, and
/// `TSInstantiationExpression` per ADR-0002 §4. Also unwraps
/// `ParenthesizedExpression` so `(undefined as any)` and
/// `(((x as A) satisfies B)!)` collapse to their leaf expressions.
/// Recursive.
#[must_use]
pub fn unwrap_ts_expr<'a, 'b>(expr: &'a Expression<'b>) -> &'a Expression<'b> {
    match expr {
        Expression::TSAsExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSNonNullExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSTypeAssertion(inner) => unwrap_ts_expr(&inner.expression),
        Expression::TSInstantiationExpression(inner) => unwrap_ts_expr(&inner.expression),
        Expression::ParenthesizedExpression(inner) => unwrap_ts_expr(&inner.expression),
        other => other,
    }
}

/// `true` for top-level TS statements that ADR-0002 §4 marks as
/// "erased" — they produce no bytecode and are not errors.
fn is_erased_ts_statement(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::TSTypeAliasDeclaration(_)
        | Statement::TSInterfaceDeclaration(_)
        | Statement::TSImportEqualsDeclaration(_) => true,

        // `declare function f();` and friends.
        Statement::FunctionDeclaration(f) if f.declare => true,
        Statement::ClassDeclaration(c) if c.declare => true,
        Statement::VariableDeclaration(v) if v.declare => true,

        // `import type { X } from "y"` / `import { type X } from "y"`
        // — when the whole import is type-only the declaration is
        // erased; otherwise this slice does not yet support imports.
        Statement::ImportDeclaration(d) if d.import_kind.is_type() => true,

        // `export type { ... }` / `export type X = ...`
        Statement::ExportNamedDeclaration(d) if d.export_kind.is_type() => true,
        Statement::ExportAllDeclaration(d) if d.export_kind.is_type() => true,

        // `declare module "..." { ... }` and `declare namespace N { ... }`.
        Statement::TSModuleDeclaration(m) if m.declare => true,

        _ => false,
    }
}

/// `Some((node, span))` for top-level TS statements that ADR-0002 §4
/// marks as "diagnosed" — produce a structured `TS_UNSUPPORTED`.
fn rejected_ts_statement(stmt: &Statement<'_>) -> Option<(&'static str, (u32, u32))> {
    use oxc_span::GetSpan;
    match stmt {
        Statement::TSEnumDeclaration(d) => Some(("TSEnumDeclaration", (d.span.start, d.span.end))),
        // Non-`declare` namespace with a runtime body.
        Statement::TSModuleDeclaration(d) if !d.declare => {
            Some(("TSModuleDeclaration", (d.span.start, d.span.end)))
        }
        Statement::ClassDeclaration(c) if !c.decorators.is_empty() => {
            let s = c.decorators[0].span();
            Some(("Decorator", (s.start, s.end)))
        }
        _ => None,
    }
}

fn stmt_kind_name(stmt: &Statement<'_>) -> &'static str {
    match stmt {
        Statement::EmptyStatement(_) => "EmptyStatement",
        Statement::ExpressionStatement(_) => "ExpressionStatement",
        Statement::VariableDeclaration(_) => "VariableDeclaration",
        Statement::FunctionDeclaration(_) => "FunctionDeclaration",
        Statement::ClassDeclaration(_) => "ClassDeclaration",
        Statement::IfStatement(_) => "IfStatement",
        Statement::ForStatement(_) => "ForStatement",
        Statement::WhileStatement(_) => "WhileStatement",
        Statement::DoWhileStatement(_) => "DoWhileStatement",
        Statement::ReturnStatement(_) => "ReturnStatement",
        Statement::BlockStatement(_) => "BlockStatement",
        Statement::TSEnumDeclaration(_) => "TSEnumDeclaration",
        Statement::TSInterfaceDeclaration(_) => "TSInterfaceDeclaration",
        Statement::TSTypeAliasDeclaration(_) => "TSTypeAliasDeclaration",
        Statement::TSModuleDeclaration(_) => "TSModuleDeclaration",
        Statement::ImportDeclaration(_) => "ImportDeclaration",
        Statement::ExportNamedDeclaration(_) => "ExportNamedDeclaration",
        Statement::ExportDefaultDeclaration(_) => "ExportDefaultDeclaration",
        Statement::ExportAllDeclaration(_) => "ExportAllDeclaration",
        _ => "Statement",
    }
}

fn stmt_span(stmt: &Statement<'_>) -> (u32, u32) {
    use oxc_span::GetSpan;
    let s = stmt.span();
    (s.start, s.end)
}

/// Concrete compiler errors.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CompileError {
    /// Parsing failed in `otter-syntax`.
    #[error("syntax: {}", .messages.join("; "))]
    Syntax {
        /// One message per OXC parser diagnostic.
        messages: Vec<String>,
    },
    /// The AST node is recognized but not supported by this slice.
    #[error("unsupported {node} at offset {}-{}", .span.0, .span.1)]
    Unsupported {
        /// AST node kind name.
        node: String,
        /// Source span of the offending node.
        span: (u32, u32),
    },
    /// A TypeScript construct is intentionally rejected by the
    /// foundation per ADR-0002 §4 (e.g., `enum`, runtime
    /// `namespace`, decorators).
    #[error("typescript construct {node} is not supported in foundation")]
    TypeScriptUnsupported {
        /// AST node kind name.
        node: String,
        /// Source span of the offending node.
        span: (u32, u32),
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_syntax::parse;

    #[test]
    fn empty_script_compiles() {
        let parsed = parse("", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::Return);
    }

    #[test]
    fn undefined_literal_compiles() {
        let parsed = parse("undefined;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert_eq!(main.code.len(), 2);
        assert_eq!(main.code[0].op, Op::LoadUndefined);
        assert_eq!(main.code[1].op, Op::Return);
    }

    #[test]
    fn unsupported_statement_rejects() {
        // `try`/`catch` is not yet supported in the foundation
        // subset; expect the Unsupported diagnostic with any
        // descriptive node name.
        let parsed = parse("try {} catch (e) {}", SyntaxSourceKind::TypeScript).unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        assert!(matches!(err, CompileError::Unsupported { .. }));
    }

    #[test]
    fn type_alias_is_erased() {
        let parsed = parse(
            "type Foo = number; undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        // LoadUndefined for the body + Return.
        let main = module.main();
        assert_eq!(main.code.len(), 2);
    }

    #[test]
    fn interface_is_erased() {
        let parsed = parse(
            "interface I { x: number; } undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn declare_function_is_erased() {
        let parsed = parse(
            "declare function foo(): void; undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn import_type_is_erased() {
        let parsed = parse(
            "import type { Foo } from \"./foo\"; undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn as_expression_unwraps_to_undefined() {
        let parsed = parse("(undefined as any);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        // `(undefined as any)` is statement-level; LoadUndefined + Return.
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn satisfies_expression_unwraps_to_undefined() {
        let parsed = parse(
            "(undefined satisfies unknown);",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn non_null_unwraps_to_undefined() {
        let parsed = parse("undefined!;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn enum_is_rejected_with_ts_unsupported() {
        let parsed = parse("enum E { A }", SyntaxSourceKind::TypeScript).unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        match err {
            CompileError::TypeScriptUnsupported { node, .. } => {
                assert_eq!(node, "TSEnumDeclaration");
            }
            other => panic!("expected TypeScriptUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn namespace_with_runtime_body_is_rejected() {
        let parsed = parse(
            "namespace N { export const x = 1; }",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        assert!(matches!(err, CompileError::TypeScriptUnsupported { .. }));
    }

    #[test]
    fn declared_namespace_is_erased() {
        let parsed = parse(
            "declare namespace N { function f(): void; } undefined;",
            SyntaxSourceKind::TypeScript,
        )
        .unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.main().code.len(), 2);
    }

    #[test]
    fn string_literal_compiles_to_load_string() {
        // Parenthesize to keep OXC from treating the bare literal
        // as a directive prologue.
        let parsed = parse("(\"abc\");", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
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
        let parsed = parse("\"a\" + \"b\";", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let main = module.main();
        assert!(main.code.iter().any(|i| i.op == Op::Add));
    }

    #[test]
    fn strict_equals_compiles_to_eq() {
        let parsed = parse("\"a\" === \"a\";", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::Equal));
    }

    #[test]
    fn numeric_literal_smi_compiles_to_load_int32() {
        let parsed = parse("(42);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadInt32));
    }

    #[test]
    fn arithmetic_lowers_to_numeric_ops() {
        let parsed = parse("1 + 2 * 3 - 4 / 5;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        let ops: Vec<Op> = module.main().code.iter().map(|i| i.op).collect();
        assert!(ops.contains(&Op::Add));
        assert!(ops.contains(&Op::Sub));
        assert!(ops.contains(&Op::Mul));
        assert!(ops.contains(&Op::Div));
    }

    #[test]
    fn unary_minus_lowers_to_neg() {
        let parsed = parse("-(5);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::Neg));
    }

    #[test]
    fn boolean_literal_lowers() {
        let parsed = parse("(true);", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadTrue));
    }

    #[test]
    fn dot_length_compiles_to_load_property() {
        // Slice 17 generalised `.length` into the same
        // `LoadProperty` opcode used for object property access;
        // the runtime keeps the string-length fast path inside
        // the dispatcher.
        let parsed = parse("\"abc\".length;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadProperty));
    }

    #[test]
    fn template_no_interpolation_compiles_to_load_string() {
        let parsed = parse("`abc`;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadString));
    }

    #[test]
    fn duplicate_string_literals_share_constant() {
        let parsed = parse("(\"abc\"); (\"abc\");", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert_eq!(module.constants.len(), 1);
    }
}
