//! Tiny end-to-end smoke harness for iterative VM validation.

use core::fmt;

use crate::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset};
use crate::call::{CallSite, CallTable, ClosureCall, DirectCall};
use crate::closure::{ClosureTable, ClosureTemplate, UpvalueId};
use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
use crate::frame::{FrameFlags, FrameLayout};
use crate::interpreter::{ExecutionResult, Interpreter, InterpreterError};
use crate::lowering::{BinaryOp, Expr, LocalId, Program, Statement, compile_module};
use crate::module::{
    Function, FunctionIndex, FunctionSideTables, FunctionTables, Module, ModuleError,
};
use crate::property::{PropertyNameId, PropertyNameTable};
use crate::string::{StringId, StringTable};
use crate::value::RegisterValue;

/// Small end-to-end smoke case for the new VM.
#[derive(Debug, Clone, PartialEq)]
pub struct SmokeCase {
    name: &'static str,
    module: Module,
    expected_return: RegisterValue,
}

impl SmokeCase {
    /// Creates a smoke case with a single prepared module.
    #[must_use]
    pub const fn new(name: &'static str, module: Module, expected_return: RegisterValue) -> Self {
        Self {
            name,
            module,
            expected_return,
        }
    }

    /// Returns the smoke-case name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the executable module.
    #[must_use]
    pub const fn module(&self) -> &Module {
        &self.module
    }

    /// Returns the expected return value.
    #[must_use]
    pub const fn expected_return(&self) -> RegisterValue {
        self.expected_return
    }
}

/// Error produced by the smoke harness.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SmokeError {
    /// The smoke module could not be constructed.
    InvalidModule(ModuleError),
    /// The interpreter failed while executing the smoke program.
    Interpreter(InterpreterError),
    /// The smoke program returned a value different from the expected one.
    UnexpectedReturn {
        /// Name of the smoke case.
        case_name: &'static str,
        /// Expected return value.
        expected: RegisterValue,
        /// Actual return value.
        actual: RegisterValue,
    },
}

impl fmt::Display for SmokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidModule(error) => error.fmt(f),
            Self::Interpreter(error) => error.fmt(f),
            Self::UnexpectedReturn {
                case_name,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "smoke case {case_name} returned unexpected value: expected {:?}, got {:?}",
                    expected, actual
                )
            }
        }
    }
}

impl std::error::Error for SmokeError {}

impl From<ModuleError> for SmokeError {
    fn from(value: ModuleError) -> Self {
        Self::InvalidModule(value)
    }
}

impl From<InterpreterError> for SmokeError {
    fn from(value: InterpreterError) -> Self {
        Self::Interpreter(value)
    }
}

/// Returns the built-in smoke cases for the current interpreter subset.
#[must_use]
pub fn default_cases() -> Vec<SmokeCase> {
    vec![
        arithmetic_case(),
        branch_loop_case(),
        lowered_case(),
        object_property_case(),
        string_array_case(),
        direct_call_case(),
        closure_case(),
    ]
}

/// Runs a smoke case and validates its return value.
pub fn run_case(case: &SmokeCase) -> Result<ExecutionResult, SmokeError> {
    let result = Interpreter::new().execute(case.module())?;
    if result.return_value() != case.expected_return() {
        return Err(SmokeError::UnexpectedReturn {
            case_name: case.name(),
            expected: case.expected_return(),
            actual: result.return_value(),
        });
    }

    Ok(result)
}

fn arithmetic_case() -> SmokeCase {
    let layout = FrameLayout::new(0, 0, 0, 4).expect("smoke layout should be valid");
    let function = Function::with_bytecode(
        Some("smoke_arithmetic"),
        layout,
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 7),
            Instruction::load_i32(BytecodeRegister::new(1), 5),
            Instruction::mul(
                BytecodeRegister::new(2),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::load_i32(BytecodeRegister::new(3), 3),
            Instruction::add(
                BytecodeRegister::new(2),
                BytecodeRegister::new(2),
                BytecodeRegister::new(3),
            ),
            Instruction::ret(BytecodeRegister::new(2)),
        ]),
    );
    let module = Module::new(Some("smoke-arithmetic"), vec![function], FunctionIndex(0))
        .expect("smoke module should be valid");

    SmokeCase::new("arithmetic", module, RegisterValue::from_i32(38))
}

fn branch_loop_case() -> SmokeCase {
    let layout = FrameLayout::new(0, 0, 0, 5).expect("smoke layout should be valid");
    let function = Function::with_bytecode(
        Some("smoke_loop"),
        layout,
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 0),
            Instruction::load_i32(BytecodeRegister::new(1), 5),
            Instruction::load_i32(BytecodeRegister::new(2), 0),
            Instruction::load_i32(BytecodeRegister::new(3), 1),
            Instruction::lt(
                BytecodeRegister::new(4),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::jump_if_false(BytecodeRegister::new(4), JumpOffset::new(3)),
            Instruction::add(
                BytecodeRegister::new(2),
                BytecodeRegister::new(2),
                BytecodeRegister::new(0),
            ),
            Instruction::add(
                BytecodeRegister::new(0),
                BytecodeRegister::new(0),
                BytecodeRegister::new(3),
            ),
            Instruction::jump(JumpOffset::new(-5)),
            Instruction::ret(BytecodeRegister::new(2)),
        ]),
    );
    let module = Module::new(Some("smoke-loop"), vec![function], FunctionIndex(0))
        .expect("smoke module should be valid");

    SmokeCase::new("branch_loop", module, RegisterValue::from_i32(10))
}

fn lowered_case() -> SmokeCase {
    let sum = LocalId::new(0);
    let index = LocalId::new(1);
    let limit = LocalId::new(2);

    let program = Program::new(
        Some("smoke-lowered"),
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
            Statement::ret(Expr::local(sum)),
        ],
    );
    let module = compile_module(&program).expect("lowered smoke program should compile");

    SmokeCase::new("lowered_loop", module, RegisterValue::from_i32(10))
}

fn object_property_case() -> SmokeCase {
    let layout = FrameLayout::new(0, 0, 0, 5).expect("smoke layout should be valid");
    let function = Function::new(
        Some("smoke_object_property"),
        layout,
        Bytecode::from(vec![
            Instruction::new_object(BytecodeRegister::new(0)),
            Instruction::load_i32(BytecodeRegister::new(1), 0),
            Instruction::load_i32(BytecodeRegister::new(2), 1),
            Instruction::load_i32(BytecodeRegister::new(3), 3),
            Instruction::set_property(
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
                PropertyNameId(0),
            ),
            Instruction::get_property(
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
                PropertyNameId(0),
            ),
            Instruction::lt(
                BytecodeRegister::new(4),
                BytecodeRegister::new(1),
                BytecodeRegister::new(3),
            ),
            Instruction::jump_if_false(BytecodeRegister::new(4), JumpOffset::new(3)),
            Instruction::add(
                BytecodeRegister::new(1),
                BytecodeRegister::new(1),
                BytecodeRegister::new(2),
            ),
            Instruction::set_property(
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
                PropertyNameId(0),
            ),
            Instruction::jump(JumpOffset::new(-6)),
            Instruction::ret(BytecodeRegister::new(1)),
        ]),
        FunctionTables::new(
            FunctionSideTables::new(
                PropertyNameTable::new(vec!["count"]),
                StringTable::default(),
                ClosureTable::default(),
                CallTable::default(),
            ),
            FeedbackTableLayout::new(vec![
                FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Branch),
                FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Branch),
                FeedbackSlotLayout::new(FeedbackSlotId(8), FeedbackKind::Arithmetic),
                FeedbackSlotLayout::new(FeedbackSlotId(9), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(10), FeedbackKind::Branch),
            ]),
            crate::deopt::DeoptTable::default(),
            crate::exception::ExceptionTable::default(),
            crate::source_map::SourceMap::default(),
        ),
    );
    let module = Module::new(
        Some("smoke-object-property"),
        vec![function],
        FunctionIndex(0),
    )
    .expect("smoke module should be valid");

    SmokeCase::new("object_property", module, RegisterValue::from_i32(3))
}

fn string_array_case() -> SmokeCase {
    let layout = FrameLayout::new(0, 0, 0, 10).expect("smoke layout should be valid");
    let function = Function::new(
        Some("smoke_string_array"),
        layout,
        Bytecode::from(vec![
            Instruction::load_string(BytecodeRegister::new(0), StringId(0)),
            Instruction::get_property(
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
                PropertyNameId(0),
            ),
            Instruction::new_array(BytecodeRegister::new(2)),
            Instruction::load_i32(BytecodeRegister::new(3), 0),
            Instruction::set_index(
                BytecodeRegister::new(2),
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
            ),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::get_index(
                BytecodeRegister::new(5),
                BytecodeRegister::new(0),
                BytecodeRegister::new(4),
            ),
            Instruction::set_index(
                BytecodeRegister::new(2),
                BytecodeRegister::new(4),
                BytecodeRegister::new(5),
            ),
            Instruction::get_index(
                BytecodeRegister::new(6),
                BytecodeRegister::new(2),
                BytecodeRegister::new(3),
            ),
            Instruction::get_property(
                BytecodeRegister::new(7),
                BytecodeRegister::new(2),
                PropertyNameId(0),
            ),
            Instruction::add(
                BytecodeRegister::new(8),
                BytecodeRegister::new(6),
                BytecodeRegister::new(7),
            ),
            Instruction::ret(BytecodeRegister::new(8)),
        ]),
        FunctionTables::new(
            FunctionSideTables::new(
                PropertyNameTable::new(vec!["length"]),
                StringTable::new(vec!["otter"]),
                ClosureTable::default(),
                CallTable::default(),
            ),
            FeedbackTableLayout::new(vec![
                FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(8), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(9), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(10), FeedbackKind::Arithmetic),
                FeedbackSlotLayout::new(FeedbackSlotId(11), FeedbackKind::Call),
            ]),
            crate::deopt::DeoptTable::default(),
            crate::exception::ExceptionTable::default(),
            crate::source_map::SourceMap::default(),
        ),
    );
    let module = Module::new(Some("smoke-string-array"), vec![function], FunctionIndex(0))
        .expect("smoke module should be valid");

    SmokeCase::new("string_array", module, RegisterValue::from_i32(7))
}

fn direct_call_case() -> SmokeCase {
    let entry_layout = FrameLayout::new(0, 0, 0, 4).expect("smoke layout should be valid");
    let helper_layout = FrameLayout::new(0, 2, 0, 1).expect("smoke layout should be valid");
    let entry = Function::new(
        Some("smoke_direct_call_entry"),
        entry_layout,
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 20),
            Instruction::load_i32(BytecodeRegister::new(1), 22),
            Instruction::call_direct(BytecodeRegister::new(2), BytecodeRegister::new(0)),
            Instruction::ret(BytecodeRegister::new(2)),
        ]),
        FunctionTables::new(
            FunctionSideTables::new(
                PropertyNameTable::default(),
                StringTable::default(),
                ClosureTable::default(),
                CallTable::new(vec![
                    None,
                    None,
                    Some(CallSite::Direct(DirectCall::new(
                        FunctionIndex(1),
                        2,
                        FrameFlags::empty(),
                    ))),
                    None,
                ]),
            ),
            FeedbackTableLayout::new(vec![
                FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
            ]),
            crate::deopt::DeoptTable::default(),
            crate::exception::ExceptionTable::default(),
            crate::source_map::SourceMap::default(),
        ),
    );
    let helper = Function::with_bytecode(
        Some("smoke_direct_call_helper"),
        helper_layout,
        Bytecode::from(vec![
            Instruction::add(
                BytecodeRegister::new(2),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::ret(BytecodeRegister::new(2)),
        ]),
    );
    let module = Module::new(
        Some("smoke-direct-call"),
        vec![entry, helper],
        FunctionIndex(0),
    )
    .expect("smoke module should be valid");

    SmokeCase::new("direct_call", module, RegisterValue::from_i32(42))
}

fn closure_case() -> SmokeCase {
    let entry_layout = FrameLayout::new(0, 0, 0, 6).expect("smoke layout should be valid");
    let closure_layout = FrameLayout::new(0, 1, 0, 4).expect("smoke layout should be valid");
    let entry = Function::new(
        Some("smoke_closure_entry"),
        entry_layout,
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 1),
            Instruction::new_closure(BytecodeRegister::new(1), BytecodeRegister::new(0)),
            Instruction::load_i32(BytecodeRegister::new(2), 41),
            Instruction::call_closure(
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
                BytecodeRegister::new(2),
            ),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::call_closure(
                BytecodeRegister::new(5),
                BytecodeRegister::new(1),
                BytecodeRegister::new(4),
            ),
            Instruction::ret(BytecodeRegister::new(5)),
        ]),
        FunctionTables::new(
            FunctionSideTables::new(
                PropertyNameTable::default(),
                StringTable::default(),
                ClosureTable::new(vec![
                    None,
                    Some(ClosureTemplate::new(FunctionIndex(1), 1)),
                    None,
                    None,
                    None,
                    None,
                    None,
                ]),
                CallTable::new(vec![
                    None,
                    None,
                    None,
                    Some(CallSite::Closure(ClosureCall::new(1, FrameFlags::empty()))),
                    None,
                    Some(CallSite::Closure(ClosureCall::new(1, FrameFlags::empty()))),
                    None,
                ]),
            ),
            FeedbackTableLayout::new(vec![
                FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
            ]),
            crate::deopt::DeoptTable::default(),
            crate::exception::ExceptionTable::default(),
            crate::source_map::SourceMap::default(),
        ),
    );
    let closure = Function::with_bytecode(
        Some("smoke_closure_helper"),
        closure_layout,
        Bytecode::from(vec![
            Instruction::get_upvalue(BytecodeRegister::new(1), UpvalueId(0)),
            Instruction::add(
                BytecodeRegister::new(2),
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
            ),
            Instruction::set_upvalue(BytecodeRegister::new(2), UpvalueId(0)),
            Instruction::get_upvalue(BytecodeRegister::new(3), UpvalueId(0)),
            Instruction::ret(BytecodeRegister::new(3)),
        ]),
    );
    let module = Module::new(
        Some("smoke-closure"),
        vec![entry, closure],
        FunctionIndex(0),
    )
    .expect("smoke module should be valid");

    SmokeCase::new("closure", module, RegisterValue::from_i32(43))
}

#[cfg(test)]
mod tests {
    use crate::value::RegisterValue;

    use super::{default_cases, run_case};

    #[test]
    fn default_smoke_cases_execute_end_to_end() {
        let cases = default_cases();

        assert_eq!(cases.len(), 7);

        for case in &cases {
            let result = run_case(case).expect("default smoke case should succeed");
            assert_eq!(result.return_value(), case.expected_return());
        }
    }

    #[test]
    fn branch_loop_case_has_expected_result() {
        let case = default_cases()
            .into_iter()
            .find(|case| case.name() == "branch_loop")
            .expect("branch loop smoke case should exist");

        assert_eq!(case.expected_return(), RegisterValue::from_i32(10));
    }

    #[test]
    fn lowered_case_has_expected_result() {
        let case = default_cases()
            .into_iter()
            .find(|case| case.name() == "lowered_loop")
            .expect("lowered smoke case should exist");

        assert_eq!(case.expected_return(), RegisterValue::from_i32(10));
    }

    #[test]
    fn object_property_case_has_expected_result() {
        let case = default_cases()
            .into_iter()
            .find(|case| case.name() == "object_property")
            .expect("object property smoke case should exist");

        assert_eq!(case.expected_return(), RegisterValue::from_i32(3));
    }

    #[test]
    fn string_array_case_has_expected_result() {
        let case = default_cases()
            .into_iter()
            .find(|case| case.name() == "string_array")
            .expect("string/array smoke case should exist");

        assert_eq!(case.expected_return(), RegisterValue::from_i32(7));
    }

    #[test]
    fn direct_call_case_has_expected_result() {
        let case = default_cases()
            .into_iter()
            .find(|case| case.name() == "direct_call")
            .expect("direct call smoke case should exist");

        assert_eq!(case.expected_return(), RegisterValue::from_i32(42));
    }

    #[test]
    fn closure_case_has_expected_result() {
        let case = default_cases()
            .into_iter()
            .find(|case| case.name() == "closure")
            .expect("closure smoke case should exist");

        assert_eq!(case.expected_return(), RegisterValue::from_i32(43));
    }
}
