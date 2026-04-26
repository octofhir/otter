//! AST → bytecode lowering.
//!
//! The compiler walks the OXC AST produced by `otter-syntax` and
//! emits an [`otter_bytecode::BytecodeModule`]. The harness slice
//! (task 07) supports only:
//!
//! - empty scripts;
//! - statements that are an [`undefined`](
//!   https://tc39.es/ecma262/#sec-undefined) literal expression.
//!
//! Slice tasks `09`–`13` extend the surface. Anything outside the
//! supported set returns [`CompileError::Unsupported`] with a clear
//! diagnostic referencing the syntactic node.
//!
//! # Contents
//! - [`compile`] — entry point.
//! - [`CompileError`] — concrete error enum.
//!
//! # Invariants
//! - The function table starts with `<main>` at index 0.
//! - Every emitted instruction has a matching `SpanEntry` so source
//!   spans survive into diagnostics and stack traces (foundation
//!   plan §M2).
//! - TypeScript erasure happens here for the foundation subset
//!   (ADR-0002 §4); only the subset needed for the harness fixtures
//!   is implemented in this slice.
//!
//! # See also
//! - [`docs/new-engine/adr/0002-oxc-frontend.md`](
//!     ../../../docs/new-engine/adr/0002-oxc-frontend.md
//!   )

use otter_bytecode::{
    BytecodeModule, Function, Instruction, Op, Operand, SourceKind as BytecodeSourceKind, SpanEntry,
};
use otter_syntax::{Parsed, SourceKind as SyntaxSourceKind};
use oxc_ast::ast::{Expression, Statement};
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

    let mut code: Vec<Instruction> = Vec::new();
    let mut spans: Vec<SpanEntry> = Vec::new();
    let mut next_pc: u32 = 0;
    let mut scratch: u16 = 0;

    for stmt in &program.body {
        match stmt {
            // Empty statements (`;`) compile to nothing.
            Statement::EmptyStatement(_) => {}

            // `expression-statement` whose expression is the
            // `undefined` identifier compiles to a single
            // `LOAD_UNDEFINED r0`.
            //
            // Anything else is rejected at this slice.
            Statement::ExpressionStatement(es) => {
                if is_undefined_identifier(&es.expression) {
                    let dst = scratch;
                    scratch = scratch.max(1);
                    let pc = next_pc;
                    code.push(Instruction {
                        pc,
                        op: Op::LoadUndefined,
                        operands: vec![Operand::Register(dst)],
                    });
                    spans.push(SpanEntry {
                        pc,
                        span: (es.span.start, es.span.end),
                    });
                    next_pc += 1;
                } else {
                    return Err(CompileError::Unsupported {
                        node: "ExpressionStatement (non-`undefined`)".to_string(),
                        span: (es.span.start, es.span.end),
                    });
                }
            }

            other => {
                return Err(CompileError::Unsupported {
                    node: stmt_kind_name(other).to_string(),
                    span: stmt_span(other),
                });
            }
        }
    }

    // Always end with `RETURN r0`. The fallback completion value is
    // `undefined`, which we materialize via a synthesized
    // `LOAD_UNDEFINED r0` if the body emitted no real value.
    let return_reg = if scratch == 0 {
        let pc = next_pc;
        code.push(Instruction {
            pc,
            op: Op::LoadUndefined,
            operands: vec![Operand::Register(0)],
        });
        spans.push(SpanEntry {
            pc,
            span: (program.span.start, program.span.end),
        });
        next_pc += 1;
        scratch = 1;
        0
    } else {
        0
    };
    let return_pc = next_pc;
    code.push(Instruction {
        pc: return_pc,
        op: Op::Return,
        operands: vec![Operand::Register(return_reg)],
    });
    spans.push(SpanEntry {
        pc: return_pc,
        span: (program.span.start, program.span.end),
    });

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
            scratch,
            code,
            spans,
        }],
    })
}

fn is_undefined_identifier(expr: &Expression<'_>) -> bool {
    matches!(expr, Expression::Identifier(id) if id.name.as_str() == "undefined")
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
}
