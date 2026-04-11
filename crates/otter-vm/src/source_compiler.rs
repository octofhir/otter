//! # Otter source → bytecode compiler
//!
//! Compiles parsed ECMAScript ASTs (via `oxc_ast`) into the VM's bytecode
//! representation, producing linked `Module` objects ready for
//! [`crate::interpreter::Interpreter::run`].
//!
//! ## Submodule index
//!
//! | Module                    | Purpose                                                       |
//! |---------------------------|---------------------------------------------------------------|
//! | `shared`                  | `FunctionCompiler` struct, `Binding`, `ScopeFrame`, shared types. |
//! | `compiler`                | Function body compilation: statements, binding, registers.   |
//! | `classes`                 | Class declaration lowering (body/fields/methods/ctors).       |
//! | `statements`              | Loops, switch, try/catch/finally, break/continue, labels.     |
//! | `expressions`             | Literals, operators, plain member access, object/array lits. |
//! | `calls`                   | Call / super / new / member-update expressions.               |
//! | `functions_and_classes`   | Function, arrow, and class expression code-gen.               |
//! | `destructuring`           | Binding + assignment pattern lowering.                        |
//! | `assignment`              | `=`, `+=`, `&&=`, `??=` and friends.                          |
//! | `modules`                 | `import` / `export` statement lowering.                       |
//! | `module_compiler`         | `ModuleCompiler` orchestration, top-level FunctionCompiler.   |
//! | `ast`                     | Validators, hoisting collection, private-name checks.         |
//! | `source_mapper`           | PC → source-location mapping.                                 |
//! | `line_index`              | UTF-8 byte offset → (line, column) lookup.                    |

use std::collections::BTreeMap;

use oxc_ast::ast::{
    Argument, AssignmentOperator, AssignmentTarget, AssignmentTargetMaybeDefault,
    AssignmentTargetProperty, BinaryOperator, BindingPattern, ComputedMemberExpression, Expression,
    ForStatementLeft, Function, LogicalOperator, ObjectPropertyKind, Program as AstProgram,
    PropertyKey, PropertyKind, SimpleAssignmentTarget, Statement as AstStatement,
    StaticMemberExpression, UnaryOperator, UpdateOperator, VariableDeclarationKind,
};

use std::rc::Rc;
use std::sync::Arc;

use crate::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset, Opcode};
use crate::call::{CallSite, CallTable, ClosureCall};
use crate::closure::{ClosureTable, ClosureTemplate, UpvalueId};
use crate::deopt::DeoptTable;
use crate::exception::{ExceptionHandler, ExceptionTable};
use crate::feedback::FeedbackTableLayout;
use crate::float::{FloatId, FloatTable};
use crate::frame::{FrameFlags, FrameLayout, RegisterIndex};
use crate::module::{
    Function as VmFunction, FunctionIndex, FunctionSideTables, FunctionTables, Module,
};
use crate::property::{PropertyNameId, PropertyNameTable};
use crate::source::{LoweringMode, SourceLoweringError};
use crate::source_map::SourceMap;
use crate::string::{StringId, StringTable};

mod assignment;
pub(crate) mod ast;
mod calls;
mod classes;
mod compiler;
mod destructuring;
mod expressions;
mod functions_and_classes;
pub(crate) mod line_index;
mod module_compiler;
mod modules;
mod shared;
pub(crate) mod source_mapper;
mod statements;

use module_compiler::ModuleCompiler;
use source_mapper::SourceMapper;

/// Bundle of inputs passed to `compile_program_to_module`.
///
/// Exists because the caller (`source.rs`) already holds both the `Module`
/// input source text and (optionally) an oxc V3 sourcemap back to the
/// original source when the input was TypeScript. Passing these through as
/// a struct keeps future additions (e.g., source origin metadata) typed.
pub(crate) struct ProgramInput<'a> {
    pub(crate) program: &'a AstProgram<'a>,
    pub(crate) source_url: &'a str,
    pub(crate) mode: LoweringMode,
    /// The generated JS that was parsed into `program`. For `.js` inputs this
    /// is the original user source; for `.ts` inputs it's the oxc codegen
    /// output. Used by the `SourceMapper` to turn spans into `(line, col)`.
    pub(crate) generated_js: &'a str,
    /// The **original** source text the developer wrote (TS or JS). Stored
    /// on the produced `Module` so diagnostics can render snippets that match
    /// the user's file byte-for-byte.
    pub(crate) original_source: Arc<str>,
    /// V3 sourcemap from `generated_js` back to `original_source`. `None` for
    /// `.js` inputs (the first hop is the identity).
    pub(crate) oxc_map: Option<oxc_sourcemap::SourceMap>,
}

pub(crate) fn compile_program_to_module(
    input: ProgramInput<'_>,
) -> Result<Module, SourceLoweringError> {
    let mapper = match input.oxc_map {
        Some(map) => SourceMapper::with_oxc_map(input.generated_js, map),
        None => SourceMapper::identity(input.generated_js),
    };
    ModuleCompiler::new(
        input.source_url,
        input.mode,
        Rc::new(mapper),
        input.original_source,
    )
    .compile(input.program)
}
