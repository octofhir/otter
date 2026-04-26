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

use std::collections::HashMap;

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

    let mut cx = FunctionContext::default();
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

    let kind = match parsed.kind {
        SyntaxSourceKind::JavaScript => BytecodeSourceKind::JavaScript,
        SyntaxSourceKind::TypeScript => BytecodeSourceKind::TypeScript,
    };

    Ok(BytecodeModule {
        module: module_specifier.to_string(),
        source_kind: kind,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            span: (program.span.start, program.span.end),
            locals: 0,
            scratch: cx.scratch,
            code: cx.code,
            spans: cx.spans,
        }],
        constants: cx.constants,
    })
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
#[derive(Debug, Default)]
struct FunctionContext {
    code: Vec<Instruction>,
    spans: Vec<SpanEntry>,
    constants: Vec<Constant>,
    next_pc: u32,
    scratch: u16,
    /// Stack of lexical scopes. Index 0 is the function-body
    /// scope.
    scopes: Vec<Scope>,
    /// Stack of enclosing loops; the innermost is on top.
    loops: Vec<LoopFrame>,
}

impl FunctionContext {
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
        scope
            .bindings
            .insert(name.to_string(), BindingInfo { reg, is_const });
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
        // Linear scan — fine for the harness slice; replace with a
        // hashmap once constant tables grow.
        let utf16: Vec<u16> = value.encode_utf16().collect();
        for (i, c) in self.constants.iter().enumerate() {
            if let Constant::String { utf16: existing } = c
                && existing == &utf16
            {
                return i as u32;
            }
        }
        self.constants.push(Constant::String { utf16 });
        (self.constants.len() - 1) as u32
    }

    fn intern_number_constant(&mut self, value: f64) -> u32 {
        let bits = value.to_bits();
        for (i, c) in self.constants.iter().enumerate() {
            if let Constant::Number { bits: existing } = c
                && *existing == bits
            {
                return i as u32;
            }
        }
        self.constants.push(Constant::Number { bits });
        (self.constants.len() - 1) as u32
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
                let init_reg = match &declarator.init {
                    Some(init) => compile_expr(cx, init, span)?,
                    None => {
                        // No initializer → undefined.
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
    }
    Ok(())
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
                    cx.emit(
                        Op::LoadLocal,
                        vec![Operand::Register(dst), Operand::Imm32(info.reg as i32)],
                        span,
                    );
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
            // Foundation subset: only plain `=` to a bound
            // identifier. Compound assignments (`+=`, `||=`, …)
            // and member-target assignments are deferred.
            if !matches!(a.operator, AssignmentOperator::Assign) {
                return Err(CompileError::Unsupported {
                    node: format!("AssignmentExpression ({:?})", a.operator),
                    span,
                });
            }
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

        Expression::StaticMemberExpression(m) if m.property.name.as_str() == "length" => {
            // Only string-typed receivers (literal / nested
            // expressions whose static type the compiler trusts) at
            // this slice. We just compile the receiver and emit
            // LOAD_LENGTH; the VM raises TypeMismatch if the
            // receiver isn't a string at run time.
            let span = (m.span.start, m.span.end);
            let receiver = compile_expr(cx, &m.object, span)?;
            let dst = cx.alloc_scratch();
            cx.emit(
                Op::LoadLength,
                vec![Operand::Register(dst), Operand::Register(receiver)],
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
                Op::GetStringIndex,
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

        other => Err(CompileError::Unsupported {
            node: format!("Expression ({})", expr_kind_name(other)),
            span: expr_span(other),
        }),
    }
}

/// Lower `receiver.method(args...)` where `method` is a static
/// member access. The runtime resolves the method via the
/// `String.prototype` intrinsic table; non-string receivers raise
/// `TypeMismatch`. Other call shapes (free calls, computed-method
/// access, `new`, spread) are deferred to later slices.
fn compile_method_call(
    cx: &mut FunctionContext,
    call: &oxc_ast::ast::CallExpression<'_>,
) -> Result<u16, CompileError> {
    let span = (call.span.start, call.span.end);
    let callee = unwrap_ts_expr(&call.callee);
    let Expression::StaticMemberExpression(member) = callee else {
        return Err(CompileError::Unsupported {
            node: "CallExpression (non-method call)".to_string(),
            span,
        });
    };

    let receiver_reg = compile_expr(cx, &member.object, span)?;
    let name = member.property.name.as_str();
    let name_idx = cx.intern_string_constant(name);

    // Compile each argument and collect register handles.
    let mut arg_regs: Vec<u16> = Vec::with_capacity(call.arguments.len());
    for arg in &call.arguments {
        match arg {
            oxc_ast::ast::Argument::SpreadElement(s) => {
                return Err(CompileError::Unsupported {
                    node: "Argument::SpreadElement".to_string(),
                    span: (s.span.start, s.span.end),
                });
            }
            other => {
                // The Argument enum covers every Expression variant;
                // map it back through the Expression view.
                let expr = other.to_expression();
                let reg = compile_expr(cx, expr, span)?;
                arg_regs.push(reg);
            }
        }
    }

    let dst = cx.alloc_scratch();
    let mut operands: Vec<Operand> = Vec::with_capacity(4 + arg_regs.len());
    operands.push(Operand::Register(dst));
    operands.push(Operand::Register(receiver_reg));
    operands.push(Operand::ConstIndex(name_idx));
    operands.push(Operand::ConstIndex(arg_regs.len() as u32));
    operands.extend(arg_regs.into_iter().map(Operand::Register));
    cx.emit(Op::CallStringMethod, operands, span);
    Ok(dst)
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
        let parsed = parse("if (true) {}", SyntaxSourceKind::TypeScript).unwrap();
        let err = compile(&parsed, "test.ts").unwrap_err();
        match err {
            CompileError::Unsupported { node, .. } => {
                assert_eq!(node, "IfStatement");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
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
    fn dot_length_compiles_to_load_length() {
        let parsed = parse("\"abc\".length;", SyntaxSourceKind::TypeScript).unwrap();
        let module = compile(&parsed, "test.ts").unwrap();
        assert!(module.main().code.iter().any(|i| i.op == Op::LoadLength));
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
