use std::collections::BTreeMap;

use oxc_ast::ast::{
    Argument, AssignmentOperator, AssignmentTarget, BinaryOperator, BindingPattern,
    ComputedMemberExpression, Expression, ForStatementLeft, Function, LogicalOperator,
    ObjectPropertyKind, Program as AstProgram, PropertyKey, PropertyKind, SimpleAssignmentTarget,
    Statement as AstStatement, StaticMemberExpression, UnaryOperator, UpdateOperator,
    VariableDeclarationKind,
};

use crate::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset, Opcode};
use crate::call::{CallSite, CallTable, ClosureCall};
use crate::closure::{ClosureTable, ClosureTemplate, UpvalueId};
use crate::deopt::DeoptTable;
use crate::exception::{ExceptionHandler, ExceptionTable};
use crate::feedback::FeedbackTableLayout;
use crate::frame::{FrameFlags, FrameLayout, RegisterIndex};
use crate::module::{
    Function as VmFunction, FunctionIndex, FunctionSideTables, FunctionTables, Module,
};
use crate::property::{PropertyNameId, PropertyNameTable};
use crate::source::{LoweringMode, SourceLoweringError};
use crate::source_map::SourceMap;
use crate::string::{StringId, StringTable};

mod ast;
mod compiler;
mod expressions;
mod module_compiler;
mod shared;

use module_compiler::ModuleCompiler;

pub(crate) fn compile_program_to_module(
    program: &AstProgram<'_>,
    source_url: &str,
    mode: LoweringMode,
) -> Result<Module, SourceLoweringError> {
    ModuleCompiler::new(source_url, mode).compile(program)
}
