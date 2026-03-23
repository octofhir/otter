//! Tiny lowering bridge for the first executable subset of the new VM.

use core::fmt;

use crate::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset, Opcode};
use crate::frame::RegisterIndex;
use crate::module::{Function, FunctionIndex, Module, ModuleError};

/// Stable local identifier inside the tiny lowering subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalId(RegisterIndex);

impl LocalId {
    /// Creates a local identifier.
    #[must_use]
    pub const fn new(index: RegisterIndex) -> Self {
        Self(index)
    }

    /// Returns the local index.
    #[must_use]
    pub const fn index(self) -> RegisterIndex {
        self.0
    }
}

/// Binary operations supported by the tiny lowering subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// Integer addition.
    Add,
    /// Integer subtraction.
    Sub,
    /// Integer multiplication.
    Mul,
    /// Integer division.
    Div,
    /// Equality comparison.
    Eq,
    /// Less-than comparison.
    Lt,
}

/// Tiny expression tree for the first lowering bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// Read a local slot.
    Local(LocalId),
    /// Inline `i32` constant.
    I32(i32),
    /// Inline boolean constant.
    Bool(bool),
    /// Binary expression.
    Binary {
        /// Operation to apply.
        op: BinaryOp,
        /// Left-hand operand.
        lhs: Box<Expr>,
        /// Right-hand operand.
        rhs: Box<Expr>,
    },
}

impl Expr {
    /// Creates a local expression.
    #[must_use]
    pub const fn local(local: LocalId) -> Self {
        Self::Local(local)
    }

    /// Creates an integer literal.
    #[must_use]
    pub const fn i32(value: i32) -> Self {
        Self::I32(value)
    }

    /// Creates a boolean literal.
    #[must_use]
    pub const fn bool(value: bool) -> Self {
        Self::Bool(value)
    }

    /// Creates a binary expression.
    #[must_use]
    pub fn binary(op: BinaryOp, lhs: Self, rhs: Self) -> Self {
        Self::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }
}

/// Tiny statement tree for the first lowering bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statement {
    /// Assign a value to a local slot.
    Assign {
        /// Local to update.
        target: LocalId,
        /// Value expression.
        value: Expr,
    },
    /// Conditional execution.
    If {
        /// Condition expression.
        condition: Expr,
        /// Then branch body.
        then_body: Box<[Statement]>,
        /// Else branch body.
        else_body: Box<[Statement]>,
    },
    /// Loop while the condition stays truthy.
    While {
        /// Condition expression.
        condition: Expr,
        /// Loop body.
        body: Box<[Statement]>,
    },
    /// Return a value.
    Return(Expr),
}

impl Statement {
    /// Creates an assignment statement.
    #[must_use]
    pub const fn assign(target: LocalId, value: Expr) -> Self {
        Self::Assign { target, value }
    }

    /// Creates an `if` statement.
    #[must_use]
    pub fn if_(condition: Expr, then_body: Vec<Statement>, else_body: Vec<Statement>) -> Self {
        Self::If {
            condition,
            then_body: then_body.into_boxed_slice(),
            else_body: else_body.into_boxed_slice(),
        }
    }

    /// Creates a `while` statement.
    #[must_use]
    pub fn while_(condition: Expr, body: Vec<Statement>) -> Self {
        Self::While {
            condition,
            body: body.into_boxed_slice(),
        }
    }

    /// Creates a return statement.
    #[must_use]
    pub const fn ret(value: Expr) -> Self {
        Self::Return(value)
    }
}

/// Tiny structured program compiled into one `otter-vm` function/module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    name: Option<Box<str>>,
    local_count: RegisterIndex,
    body: Box<[Statement]>,
}

impl Program {
    /// Creates a structured program for the tiny lowering subset.
    #[must_use]
    pub fn new(
        name: Option<impl Into<Box<str>>>,
        local_count: RegisterIndex,
        body: Vec<Statement>,
    ) -> Self {
        Self {
            name: name.map(Into::into),
            local_count,
            body: body.into_boxed_slice(),
        }
    }

    /// Returns the program name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Error produced while lowering the tiny structured subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoweringError {
    /// The program referenced a local outside the declared local count.
    LocalOutOfBounds,
    /// The program can fall off the end without an explicit return.
    MissingReturn,
    /// The generated jump target does not fit into the bytecode encoding.
    JumpOutOfRange,
    /// The generated module shape was invalid.
    InvalidModule(ModuleError),
}

impl fmt::Display for LoweringError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalOutOfBounds => f.write_str("lowering referenced a local outside the frame"),
            Self::MissingReturn => {
                f.write_str("lowered program can fall through without an explicit return")
            }
            Self::JumpOutOfRange => {
                f.write_str("lowered jump target does not fit into the bytecode format")
            }
            Self::InvalidModule(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for LoweringError {}

impl From<ModuleError> for LoweringError {
    fn from(value: ModuleError) -> Self {
        Self::InvalidModule(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValueLocation {
    register: BytecodeRegister,
    is_temp: bool,
}

impl ValueLocation {
    const fn local(register: BytecodeRegister) -> Self {
        Self {
            register,
            is_temp: false,
        }
    }

    const fn temp(register: BytecodeRegister) -> Self {
        Self {
            register,
            is_temp: true,
        }
    }
}

#[derive(Debug)]
struct LoweringContext {
    local_count: RegisterIndex,
    next_temp: RegisterIndex,
    max_temp: RegisterIndex,
    instructions: Vec<Instruction>,
}

impl LoweringContext {
    fn new(local_count: RegisterIndex) -> Self {
        Self {
            local_count,
            next_temp: 0,
            max_temp: 0,
            instructions: Vec::new(),
        }
    }

    fn finish(self, name: Option<&str>) -> Result<Function, LoweringError> {
        let frame_layout = crate::frame::FrameLayout::new(0, 0, self.local_count, self.max_temp)
            .map_err(|_| LoweringError::LocalOutOfBounds)?;

        Ok(Function::with_bytecode(
            name,
            frame_layout,
            Bytecode::from(self.instructions),
        ))
    }

    fn lower_block(&mut self, statements: &[Statement]) -> Result<bool, LoweringError> {
        let mut terminated = false;

        for statement in statements {
            if terminated {
                break;
            }
            terminated = self.lower_statement(statement)?;
        }

        Ok(terminated)
    }

    fn lower_statement(&mut self, statement: &Statement) -> Result<bool, LoweringError> {
        match statement {
            Statement::Assign { target, value } => {
                let target = self.local_register(*target)?;
                let value = self.lower_expr(value)?;
                if value.register != target {
                    self.instructions
                        .push(Instruction::move_(target, value.register));
                }
                self.release(value);
                Ok(false)
            }
            Statement::If {
                condition,
                then_body,
                else_body,
            } => {
                let condition = self.lower_expr(condition)?;
                let jump_to_else =
                    self.emit_conditional_placeholder(Opcode::JumpIfFalse, condition.register);
                self.release(condition);

                let then_terminated = self.lower_block(then_body)?;
                if else_body.is_empty() {
                    self.patch_jump(jump_to_else, self.instructions.len())?;
                    return Ok(false);
                }

                let jump_to_end = self.emit_jump_placeholder();
                self.patch_jump(jump_to_else, self.instructions.len())?;
                let else_terminated = self.lower_block(else_body)?;
                self.patch_jump(jump_to_end, self.instructions.len())?;

                Ok(then_terminated && else_terminated)
            }
            Statement::While { condition, body } => {
                let loop_start = self.instructions.len();
                let condition = self.lower_expr(condition)?;
                let exit_jump =
                    self.emit_conditional_placeholder(Opcode::JumpIfFalse, condition.register);
                self.release(condition);
                let _ = self.lower_block(body)?;
                self.emit_relative_jump(loop_start)?;
                self.patch_jump(exit_jump, self.instructions.len())?;
                Ok(false)
            }
            Statement::Return(value) => {
                let value = self.lower_expr(value)?;
                self.instructions.push(Instruction::ret(value.register));
                self.release(value);
                Ok(true)
            }
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<ValueLocation, LoweringError> {
        match expr {
            Expr::Local(local) => Ok(ValueLocation::local(self.local_register(*local)?)),
            Expr::I32(value) => {
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::load_i32(register, *value));
                Ok(ValueLocation::temp(register))
            }
            Expr::Bool(value) => {
                let register = self.alloc_temp();
                self.instructions.push(if *value {
                    Instruction::load_true(register)
                } else {
                    Instruction::load_false(register)
                });
                Ok(ValueLocation::temp(register))
            }
            Expr::Binary { op, lhs, rhs } => self.lower_binary(*op, lhs, rhs),
        }
    }

    fn lower_binary(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Result<ValueLocation, LoweringError> {
        let lhs = self.lower_expr(lhs)?;
        let rhs = self.lower_expr(rhs)?;

        let result = if rhs.is_temp {
            rhs
        } else if lhs.is_temp {
            lhs
        } else {
            ValueLocation::temp(self.alloc_temp())
        };

        let instruction = match op {
            BinaryOp::Add => Instruction::add(result.register, lhs.register, rhs.register),
            BinaryOp::Sub => Instruction::sub(result.register, lhs.register, rhs.register),
            BinaryOp::Mul => Instruction::mul(result.register, lhs.register, rhs.register),
            BinaryOp::Div => Instruction::div(result.register, lhs.register, rhs.register),
            BinaryOp::Eq => Instruction::eq(result.register, lhs.register, rhs.register),
            BinaryOp::Lt => Instruction::lt(result.register, lhs.register, rhs.register),
        };
        self.instructions.push(instruction);

        if result.register == rhs.register {
            self.release(lhs);
        } else if result.register == lhs.register {
            self.release(rhs);
        } else {
            self.release(rhs);
            self.release(lhs);
        }

        Ok(result)
    }

    fn local_register(&self, local: LocalId) -> Result<BytecodeRegister, LoweringError> {
        if local.index() < self.local_count {
            Ok(BytecodeRegister::new(local.index()))
        } else {
            Err(LoweringError::LocalOutOfBounds)
        }
    }

    fn alloc_temp(&mut self) -> BytecodeRegister {
        let temp_index = self.next_temp;
        self.next_temp = self.next_temp.saturating_add(1);
        self.max_temp = self.max_temp.max(self.next_temp);
        BytecodeRegister::new(self.local_count.saturating_add(temp_index))
    }

    fn release(&mut self, value: ValueLocation) {
        if value.is_temp {
            self.next_temp = self.next_temp.saturating_sub(1);
        }
    }

    fn emit_jump_placeholder(&mut self) -> usize {
        let index = self.instructions.len();
        self.instructions
            .push(Instruction::jump(JumpOffset::new(0)));
        index
    }

    fn emit_conditional_placeholder(&mut self, opcode: Opcode, cond: BytecodeRegister) -> usize {
        let index = self.instructions.len();
        let instruction = match opcode {
            Opcode::JumpIfTrue => Instruction::jump_if_true(cond, JumpOffset::new(0)),
            Opcode::JumpIfFalse => Instruction::jump_if_false(cond, JumpOffset::new(0)),
            _ => panic!("conditional placeholder requires a conditional jump opcode"),
        };
        self.instructions.push(instruction);
        index
    }

    fn emit_relative_jump(&mut self, target: usize) -> Result<(), LoweringError> {
        let source = self.instructions.len();
        let offset = self.compute_offset(source, target)?;
        self.instructions.push(Instruction::jump(offset));
        Ok(())
    }

    fn patch_jump(&mut self, source: usize, target: usize) -> Result<(), LoweringError> {
        let offset = self.compute_offset(source, target)?;
        let existing = self.instructions[source];
        self.instructions[source] = match existing.opcode() {
            Opcode::Jump => Instruction::jump(offset),
            Opcode::JumpIfTrue => {
                Instruction::jump_if_true(BytecodeRegister::new(existing.a()), offset)
            }
            Opcode::JumpIfFalse => {
                Instruction::jump_if_false(BytecodeRegister::new(existing.a()), offset)
            }
            _ => return Err(LoweringError::JumpOutOfRange),
        };
        Ok(())
    }

    fn compute_offset(&self, source: usize, target: usize) -> Result<JumpOffset, LoweringError> {
        let source = i64::try_from(source).map_err(|_| LoweringError::JumpOutOfRange)?;
        let target = i64::try_from(target).map_err(|_| LoweringError::JumpOutOfRange)?;
        let offset = target - source - 1;
        let offset = i32::try_from(offset).map_err(|_| LoweringError::JumpOutOfRange)?;
        Ok(JumpOffset::new(offset))
    }
}

/// Compiles the tiny structured subset into a single-function `otter-vm` module.
pub fn compile_module(program: &Program) -> Result<Module, LoweringError> {
    let mut lowering = LoweringContext::new(program.local_count);
    if !lowering.lower_block(&program.body)? {
        return Err(LoweringError::MissingReturn);
    }

    let function = lowering.finish(program.name())?;
    Module::new(program.name(), vec![function], FunctionIndex(0)).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use crate::interpreter::Interpreter;
    use crate::value::RegisterValue;

    use super::{BinaryOp, Expr, LocalId, Program, Statement, compile_module};

    #[test]
    fn lowering_compiles_and_executes_control_flow_subset() {
        let sum = LocalId::new(0);
        let index = LocalId::new(1);
        let limit = LocalId::new(2);

        let program = Program::new(
            Some("compiled-loop"),
            3,
            vec![
                Statement::assign(sum, Expr::i32(0)),
                Statement::assign(index, Expr::i32(0)),
                Statement::assign(limit, Expr::i32(5)),
                Statement::while_(
                    Expr::binary(BinaryOp::Lt, Expr::local(index), Expr::local(limit)),
                    vec![
                        Statement::assign(
                            sum,
                            Expr::binary(BinaryOp::Add, Expr::local(sum), Expr::local(index)),
                        ),
                        Statement::assign(
                            index,
                            Expr::binary(BinaryOp::Add, Expr::local(index), Expr::i32(1)),
                        ),
                    ],
                ),
                Statement::if_(
                    Expr::binary(BinaryOp::Eq, Expr::local(sum), Expr::i32(10)),
                    vec![Statement::ret(Expr::local(sum))],
                    vec![Statement::ret(Expr::i32(-1))],
                ),
            ],
        );

        let module = compile_module(&program).expect("program should lower");
        let result = Interpreter::new()
            .execute(&module)
            .expect("lowered module should execute");

        assert_eq!(result.return_value(), RegisterValue::from_i32(10));
    }

    #[test]
    fn lowering_rejects_program_without_return() {
        let program = Program::new(
            Some("missing-return"),
            1,
            vec![Statement::assign(LocalId::new(0), Expr::i32(1))],
        );

        let result = compile_module(&program);

        assert_eq!(result, Err(super::LoweringError::MissingReturn));
    }
}
