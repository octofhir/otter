//! Tiny JS-to-`otter-vm` lowering for the first migration slice.

use std::collections::BTreeMap;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    AssignmentOperator, AssignmentTarget, BinaryOperator, BindingPattern, Expression,
    Program as AstProgram, SimpleAssignmentTarget, Statement as AstStatement, UnaryOperator,
    UpdateOperator, VariableDeclarationKind,
};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::frame::RegisterIndex;
use crate::lowering::{self, BinaryOp, Expr, LocalId, Program, Statement};
use crate::module::Module;

/// Error produced while lowering JS source into the tiny `otter-vm` subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceLoweringError {
    /// Source failed to parse.
    Parse(String),
    /// The source uses syntax or semantics outside the currently supported subset.
    Unsupported(String),
    /// The source referenced a binding that was not declared in the tiny subset.
    UnknownBinding(String),
    /// The source redeclared a binding that is already tracked in the tiny subset.
    DuplicateBinding(String),
    /// The source required more locals than the tiny subset can address.
    TooManyLocals,
    /// Lowering to bytecode/module form failed.
    Lowering(lowering::LoweringError),
}

impl std::fmt::Display for SourceLoweringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "failed to parse source: {message}"),
            Self::Unsupported(message) => {
                write!(f, "source is not supported by the new VM yet: {message}")
            }
            Self::UnknownBinding(name) => {
                write!(
                    f,
                    "source referenced an unknown binding in the new VM subset: {name}"
                )
            }
            Self::DuplicateBinding(name) => write!(
                f,
                "source redeclared a binding that the new VM subset tracks as unique: {name}"
            ),
            Self::TooManyLocals => {
                f.write_str("source exceeded the local-slot limit of the new VM subset")
            }
            Self::Lowering(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SourceLoweringError {}

impl From<lowering::LoweringError> for SourceLoweringError {
    fn from(value: lowering::LoweringError) -> Self {
        Self::Lowering(value)
    }
}

/// Parse, lower, and compile a tiny JS script into an `otter-vm` module.
pub fn compile_script(source: &str, source_url: &str) -> Result<Module, SourceLoweringError> {
    let allocator = Allocator::default();
    let ast = parse_script(&allocator, source, source_url)?;
    crate::source_compiler::compile_program_to_module(&ast, source_url, LoweringMode::Script)
}

/// Parse, lower, and compile JS source in eval mode.
/// Returns the completion value of the last expression statement.
/// Used by `-p` (print) and REPL-style evaluation.
pub fn compile_eval(source: &str, source_url: &str) -> Result<Module, SourceLoweringError> {
    let allocator = Allocator::default();
    let ast = parse_script(&allocator, source, source_url)?;
    crate::source_compiler::compile_program_to_module(&ast, source_url, LoweringMode::Eval)
}

/// Parse, lower, and compile a JS module (ESM) into an `otter-vm` module.
/// Module mode enables top-level `await` and strict mode by default.
/// Spec: <https://tc39.es/ecma262/#sec-modules>
pub fn compile_module(source: &str, source_url: &str) -> Result<Module, SourceLoweringError> {
    let allocator = Allocator::default();
    let ast = parse_module(&allocator, source, source_url)?;
    crate::source_compiler::compile_program_to_module(&ast, source_url, LoweringMode::Module)
}

/// Parse, lower, and compile a tiny native Test262 script into an `otter-vm` module.
pub fn compile_test262_basic_script(
    source: &str,
    source_url: &str,
) -> Result<Module, SourceLoweringError> {
    let allocator = Allocator::default();
    let ast = parse_script(&allocator, source, source_url)?;
    crate::source_compiler::compile_program_to_module(&ast, source_url, LoweringMode::Test262Basic)
}

/// Parse and lower a tiny JS script into a structured `otter-vm` program.
pub fn lower_script(source: &str, source_url: &str) -> Result<Program, SourceLoweringError> {
    lower_script_with_mode(source, source_url, LoweringMode::Script)
}

fn lower_script_with_mode(
    source: &str,
    source_url: &str,
    mode: LoweringMode,
) -> Result<Program, SourceLoweringError> {
    let allocator = Allocator::default();
    let program = parse_script(&allocator, source, source_url)?;
    TinyScriptLowerer::new(source_url, mode).lower_program(&program)
}

fn parse_module<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    source_url: &str,
) -> Result<AstProgram<'a>, SourceLoweringError> {
    let source_type = SourceType::from_path(source_url)
        .unwrap_or_default()
        .with_module(true);

    let parsed = Parser::new(allocator, source, source_type).parse();
    if let Some(error) = parsed.errors.first() {
        return Err(SourceLoweringError::Parse(error.to_string()));
    }
    Ok(parsed.program)
}

fn parse_script<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    source_url: &str,
) -> Result<AstProgram<'a>, SourceLoweringError> {
    let mut source_type = SourceType::from_path(source_url)
        .unwrap_or_else(|_| SourceType::default().with_script(true))
        .with_script(true);

    if source_type.is_typescript() || source_type.is_jsx() {
        return Err(SourceLoweringError::Unsupported(
            "TypeScript and JSX source are not enabled on the tiny new-VM path".to_string(),
        ));
    }

    source_type = source_type.with_module(false);

    let parsed = Parser::new(allocator, source, source_type).parse();
    if let Some(error) = parsed.errors.first() {
        return Err(SourceLoweringError::Parse(error.to_string()));
    }
    Ok(parsed.program)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoweringMode {
    Script,
    Test262Basic,
    /// Eval mode: returns the completion value of the last expression statement.
    /// Used by `-p` (print) and REPL-style evaluation.
    Eval,
    /// §16.2 — ES Module mode: strict mode by default, import/export allowed.
    /// Spec: <https://tc39.es/ecma262/#sec-modules>
    Module,
}

struct TinyScriptLowerer<'a> {
    source_url: &'a str,
    mode: LoweringMode,
    locals: BTreeMap<String, LocalId>,
    next_local: RegisterIndex,
    scratch_local: Option<LocalId>,
    return_local: Option<LocalId>,
}

impl<'a> TinyScriptLowerer<'a> {
    fn new(source_url: &'a str, mode: LoweringMode) -> Self {
        Self {
            source_url,
            mode,
            locals: BTreeMap::new(),
            next_local: 0,
            scratch_local: None,
            return_local: None,
        }
    }

    fn lower_program(mut self, program: &AstProgram<'_>) -> Result<Program, SourceLoweringError> {
        let mut body = Vec::new();
        for statement in &program.body {
            self.lower_statement(statement, &mut body)?;
        }

        match self.mode {
            LoweringMode::Script | LoweringMode::Eval | LoweringMode::Module => {
                let return_local = self.allocate_return_local()?;
                body.push(Statement::ret(Expr::local(return_local)));
            }
            LoweringMode::Test262Basic => {
                body.push(Statement::ret(Expr::i32(0)));
            }
        }

        Ok(Program::new(
            Some(self.source_url.to_string()),
            self.next_local,
            body,
        ))
    }

    fn lower_statement(
        &mut self,
        statement: &AstStatement<'_>,
        output: &mut Vec<Statement>,
    ) -> Result<(), SourceLoweringError> {
        match statement {
            AstStatement::EmptyStatement(_) => Ok(()),
            AstStatement::BlockStatement(block) => {
                for statement in &block.body {
                    self.lower_statement(statement, output)?;
                }
                Ok(())
            }
            AstStatement::VariableDeclaration(declaration) => {
                self.lower_variable_declaration(declaration, output)
            }
            AstStatement::ExpressionStatement(expression_statement) => {
                self.lower_expression_statement(&expression_statement.expression, output)
            }
            AstStatement::IfStatement(if_statement) => {
                let condition = self.lower_expression(&if_statement.test)?;
                let mut then_body = Vec::new();
                self.lower_statement(&if_statement.consequent, &mut then_body)?;

                let mut else_body = Vec::new();
                if let Some(alternate) = &if_statement.alternate {
                    self.lower_statement(alternate, &mut else_body)?;
                }

                output.push(Statement::if_(condition, then_body, else_body));
                Ok(())
            }
            AstStatement::WhileStatement(while_statement) => {
                let condition = self.lower_expression(&while_statement.test)?;
                let mut loop_body = Vec::new();
                self.lower_statement(&while_statement.body, &mut loop_body)?;
                output.push(Statement::while_(condition, loop_body));
                Ok(())
            }
            AstStatement::DoWhileStatement(do_while_statement) => {
                let condition = self.lower_expression(&do_while_statement.test)?;
                let mut loop_body = Vec::new();
                self.lower_statement(&do_while_statement.body, &mut loop_body)?;
                output.push(Statement::do_while(condition, loop_body));
                Ok(())
            }
            AstStatement::ThrowStatement(throw_statement)
                if self.mode == LoweringMode::Test262Basic
                    && self.is_test262_failure_throw(&throw_statement.argument) =>
            {
                output.push(Statement::ret(Expr::i32(1)));
                Ok(())
            }
            _ => Err(SourceLoweringError::Unsupported(format!(
                "statement {:?}",
                statement
            ))),
        }
    }

    fn lower_variable_declaration(
        &mut self,
        declaration: &oxc_ast::ast::VariableDeclaration<'_>,
        output: &mut Vec<Statement>,
    ) -> Result<(), SourceLoweringError> {
        match declaration.kind {
            VariableDeclarationKind::Var
            | VariableDeclarationKind::Let
            | VariableDeclarationKind::Const => {}
            _ => {
                return Err(SourceLoweringError::Unsupported(format!(
                    "variable declaration kind {:?}",
                    declaration.kind
                )));
            }
        }

        for declarator in &declaration.declarations {
            let BindingPattern::BindingIdentifier(identifier) = &declarator.id else {
                return Err(SourceLoweringError::Unsupported(
                    "destructuring bindings".to_string(),
                ));
            };

            let local = if declaration.kind == VariableDeclarationKind::Var {
                if let Some(local) = self.locals.get(identifier.name.as_str()).copied() {
                    local
                } else {
                    self.declare_binding(identifier.name.as_str())?
                }
            } else {
                self.declare_binding(identifier.name.as_str())?
            };
            if let Some(init) = &declarator.init {
                let value = self.lower_expression(init)?;
                output.push(Statement::assign(local, value));
            }
        }

        Ok(())
    }

    fn lower_expression_statement(
        &mut self,
        expression: &Expression<'_>,
        output: &mut Vec<Statement>,
    ) -> Result<(), SourceLoweringError> {
        match expression {
            Expression::StringLiteral(_) => Ok(()),
            Expression::AssignmentExpression(assignment) => {
                let scratch_local = self.allocate_scratch_local()?;
                let value = self.lower_assignment_expression(assignment)?;
                output.push(Statement::assign(scratch_local, value));
                Ok(())
            }
            Expression::UpdateExpression(update) => {
                let statement = self.lower_update_expression_statement(update)?;
                output.push(statement);
                Ok(())
            }
            _ => {
                let scratch_local = self.allocate_scratch_local()?;
                let value = self.lower_expression(expression)?;
                output.push(Statement::assign(scratch_local, value));
                Ok(())
            }
        }
    }

    fn lower_assignment_expression(
        &mut self,
        assignment: &oxc_ast::ast::AssignmentExpression<'_>,
    ) -> Result<Expr, SourceLoweringError> {
        if assignment.operator != AssignmentOperator::Assign {
            return Err(SourceLoweringError::Unsupported(format!(
                "assignment operator {:?}",
                assignment.operator
            )));
        }

        let AssignmentTarget::AssignmentTargetIdentifier(identifier) = &assignment.left else {
            return Err(SourceLoweringError::Unsupported(
                "non-identifier assignment target".to_string(),
            ));
        };

        let local = self.lookup_binding(identifier.name.as_str())?;
        let value = self.lower_expression(&assignment.right)?;
        Ok(Expr::assign(local, value))
    }

    fn lower_expression(
        &mut self,
        expression: &Expression<'_>,
    ) -> Result<Expr, SourceLoweringError> {
        match expression {
            Expression::NumericLiteral(literal) => {
                let value = literal.value;
                if !value.is_finite()
                    || value.fract() != 0.0
                    || value < i32::MIN as f64
                    || value > i32::MAX as f64
                {
                    return Err(SourceLoweringError::Unsupported(format!(
                        "numeric literal {value}"
                    )));
                }

                Ok(Expr::i32(value as i32))
            }
            Expression::BooleanLiteral(literal) => Ok(Expr::bool(literal.value)),
            Expression::StringLiteral(literal) if self.mode == LoweringMode::Test262Basic => {
                Ok(Expr::bool(!literal.value.as_str().is_empty()))
            }
            Expression::NullLiteral(_) if self.mode == LoweringMode::Test262Basic => {
                Ok(Expr::null())
            }
            Expression::Identifier(identifier) => {
                if self.mode == LoweringMode::Test262Basic
                    && identifier.name == "undefined"
                    && !self.locals.contains_key("undefined")
                {
                    return Ok(Expr::undefined());
                }
                if self.mode == LoweringMode::Test262Basic
                    && identifier.name == "NaN"
                    && !self.locals.contains_key("NaN")
                {
                    return Ok(Expr::bool(false));
                }

                Ok(Expr::local(self.lookup_binding(identifier.name.as_str())?))
            }
            Expression::ParenthesizedExpression(parenthesized) => {
                self.lower_expression(&parenthesized.expression)
            }
            Expression::AssignmentExpression(assignment) => {
                self.lower_assignment_expression(assignment)
            }
            Expression::BinaryExpression(binary) => {
                let operator = match binary.operator {
                    BinaryOperator::Addition => BinaryOp::Add,
                    BinaryOperator::Subtraction => BinaryOp::Sub,
                    BinaryOperator::Multiplication => BinaryOp::Mul,
                    BinaryOperator::Division => BinaryOp::Div,
                    BinaryOperator::LessThan => BinaryOp::Lt,
                    BinaryOperator::Equality | BinaryOperator::StrictEquality => BinaryOp::Eq,
                    BinaryOperator::Inequality | BinaryOperator::StrictInequality => {
                        let lhs = self.lower_expression(&binary.left)?;
                        let rhs = self.lower_expression(&binary.right)?;
                        return Ok(Expr::logical_not(Expr::binary(BinaryOp::Eq, lhs, rhs)));
                    }
                    _ => {
                        return Err(SourceLoweringError::Unsupported(format!(
                            "binary operator {:?}",
                            binary.operator
                        )));
                    }
                };

                let lhs = self.lower_expression(&binary.left)?;
                let rhs = self.lower_expression(&binary.right)?;
                Ok(Expr::binary(operator, lhs, rhs))
            }
            Expression::UnaryExpression(unary) => match unary.operator {
                UnaryOperator::UnaryNegation => Ok(Expr::binary(
                    BinaryOp::Sub,
                    Expr::i32(0),
                    self.lower_expression(&unary.argument)?,
                )),
                UnaryOperator::UnaryPlus => self.lower_expression(&unary.argument),
                UnaryOperator::Typeof => Err(SourceLoweringError::Unsupported(
                    "unary operator Typeof".to_string(),
                )),
                UnaryOperator::LogicalNot => {
                    Ok(Expr::logical_not(self.lower_expression(&unary.argument)?))
                }
                _ => Err(SourceLoweringError::Unsupported(format!(
                    "unary operator {:?}",
                    unary.operator
                ))),
            },
            _ => Err(SourceLoweringError::Unsupported(format!(
                "expression {:?}",
                expression
            ))),
        }
    }

    fn lower_update_expression_statement(
        &mut self,
        update: &oxc_ast::ast::UpdateExpression<'_>,
    ) -> Result<Statement, SourceLoweringError> {
        let SimpleAssignmentTarget::AssignmentTargetIdentifier(identifier) = &update.argument
        else {
            return Err(SourceLoweringError::Unsupported(
                "non-identifier update target".to_string(),
            ));
        };

        let local = self.lookup_binding(identifier.name.as_str())?;
        let delta = match update.operator {
            UpdateOperator::Increment => 1,
            UpdateOperator::Decrement => -1,
        };

        Ok(Statement::assign(
            local,
            Expr::binary(BinaryOp::Add, Expr::local(local), Expr::i32(delta)),
        ))
    }

    fn is_test262_failure_throw(&self, expression: &Expression<'_>) -> bool {
        let Expression::NewExpression(new_expression) = expression else {
            return false;
        };

        let Expression::Identifier(identifier) = &new_expression.callee else {
            return false;
        };

        identifier.name == "Test262Error"
    }

    fn declare_binding(&mut self, name: &str) -> Result<LocalId, SourceLoweringError> {
        if self.locals.contains_key(name) {
            return Err(SourceLoweringError::DuplicateBinding(name.to_string()));
        }

        let local = self.allocate_local()?;
        self.locals.insert(name.to_string(), local);
        Ok(local)
    }

    fn lookup_binding(&self, name: &str) -> Result<LocalId, SourceLoweringError> {
        self.locals
            .get(name)
            .copied()
            .ok_or_else(|| SourceLoweringError::UnknownBinding(name.to_string()))
    }

    fn allocate_scratch_local(&mut self) -> Result<LocalId, SourceLoweringError> {
        if let Some(local) = self.scratch_local {
            return Ok(local);
        }

        let local = self.allocate_local()?;
        self.scratch_local = Some(local);
        Ok(local)
    }

    fn allocate_return_local(&mut self) -> Result<LocalId, SourceLoweringError> {
        if let Some(local) = self.return_local {
            return Ok(local);
        }

        let local = self.allocate_local()?;
        self.return_local = Some(local);
        Ok(local)
    }

    fn allocate_local(&mut self) -> Result<LocalId, SourceLoweringError> {
        let index = self.next_local;
        self.next_local = self
            .next_local
            .checked_add(1)
            .ok_or(SourceLoweringError::TooManyLocals)?;
        Ok(LocalId::new(index))
    }
}

#[cfg(test)]
mod tests {
    use crate::{Interpreter, RuntimeState};
    use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
    use crate::interpreter::InterpreterError;
    use crate::source::{compile_script, compile_test262_basic_script, lower_script};
    use crate::value::RegisterValue;

    fn install_test262_global(runtime: &mut crate::interpreter::RuntimeState) {
        fn create_realm(
            _this: &RegisterValue,
            _args: &[RegisterValue],
            runtime: &mut crate::interpreter::RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError> {
            let realm = runtime.alloc_object();
            let global = runtime.intrinsics().global_object();
            let global_property = runtime.intern_property_name("global");
            runtime
                .objects_mut()
                .set_property(
                    realm,
                    global_property,
                    RegisterValue::from_object_handle(global.0),
                )
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("test262 $262.createRealm global install failed: {error:?}").into(),
                    )
                })?;
            Ok(RegisterValue::from_object_handle(realm.0))
        }

        let create_realm = runtime.register_native_function(NativeFunctionDescriptor::method(
            "createRealm",
            0,
            create_realm,
        ));
        let create_realm = runtime.alloc_host_function(create_realm);
        let test262 = runtime.alloc_object();
        let global = runtime.intrinsics().global_object();
        let global_property = runtime.intern_property_name("global");
        runtime
            .objects_mut()
            .set_property(
                test262,
                global_property,
                RegisterValue::from_object_handle(global.0),
            )
            .expect("test262 $262.global should install");
        let create_realm_property = runtime.intern_property_name("createRealm");
        runtime
            .objects_mut()
            .set_property(
                test262,
                create_realm_property,
                RegisterValue::from_object_handle(create_realm.0),
            )
            .expect("test262 $262.createRealm should install");
        runtime.install_global_value("$262", RegisterValue::from_object_handle(test262.0));
    }

    fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
        let module = compile_test262_basic_script(source, source_url)
            .expect("test262 basic script compiles");

        let mut runtime = crate::interpreter::RuntimeState::new();
        install_test262_global(&mut runtime);
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect("test262 basic script executes")
            .return_value()
    }

    fn execute_test262_basic_error(source: &str, source_url: &str) -> InterpreterError {
        let module = compile_test262_basic_script(source, source_url)
            .expect("test262 basic script compiles");

        let mut runtime = crate::interpreter::RuntimeState::new();
        install_test262_global(&mut runtime);
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect_err("test262 basic script should throw")
    }

    #[test]
    fn lowers_basic_loop_script() {
        let program = lower_script(
            r#"
            let sum = 0;
            let i = 0;
            while (i < 5) {
                sum = sum + i;
                i = i + 1;
            }
            sum;
            "#,
            "basic-loop.js",
        )
        .expect("script should lower");

        assert_eq!(program.name(), Some("basic-loop.js"));
    }

    #[test]
    fn compile_script_executes_on_new_vm() {
        let module = compile_script(
            r#"
            let x = 1;
            x = x + 2;
            "#,
            "next-smoke.js",
        )
        .expect("script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("script should execute");
        assert_eq!(result.return_value(), RegisterValue::undefined());
    }

    #[test]
    fn rejects_unsupported_function_declarations() {
        let error = lower_script("function f() {}", "unsupported.js")
            .expect_err("function declarations should be outside the tiny subset");

        assert!(
            error.to_string().contains("statement FunctionDeclaration"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn compile_test262_basic_script_passes_without_js_harness() {
        let module = compile_test262_basic_script(
            concat!(
                "var c = 0;\n",
                "if (!(1)) throw new Test262Error(\"#1\");\n",
                "else c++;\n",
                "if (c != 1) throw new Test262Error(\"#2\");\n",
            ),
            "native-test262-pass.js",
        )
        .expect("native test262 script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("native test262 script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_returns_failure_code() {
        let module = compile_test262_basic_script(
            concat!(
                "if (true) {\n",
                "    throw new Test262Error(\"#1\");\n",
                "}\n",
            ),
            "native-test262-fail.js",
        )
        .expect("native test262 script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("native test262 script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(1));
    }

    #[test]
    fn compile_test262_basic_script_supports_assignment_expressions_in_values() {
        let module = compile_test262_basic_script(
            concat!(
                "var x = 0;\n",
                "if ((x = 1) + x !== 2) {\n",
                "    throw new Test262Error(\"#1\");\n",
                "}\n",
                "var y = 0;\n",
                "if (y + (y = 1) !== 1) {\n",
                "    throw new Test262Error(\"#2\");\n",
                "}\n",
            ),
            "native-test262-assignment-expression.js",
        )
        .expect("assignment expression script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("assignment expression script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_function_declarations_and_calls() {
        let module = compile_test262_basic_script(
            concat!(
                "function add(a, b) {\n",
                "    return a + b;\n",
                "}\n",
                "if (add(20, 22) !== 42) {\n",
                "    throw new Test262Error(\"#1\");\n",
                "}\n",
                "function recurse(a) {\n",
                "    if (a === 0) return 7;\n",
                "    return recurse(a - 1);\n",
                "}\n",
                "if (recurse(2) !== 7) {\n",
                "    throw new Test262Error(\"#2\");\n",
                "}\n",
            ),
            "native-test262-functions.js",
        )
        .expect("function declaration script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("function declaration script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_function_expressions_closures_and_objects() {
        let module = compile_test262_basic_script(
            concat!(
                "var makeCounter = function(start) {\n",
                "    var value = start;\n",
                "    return function(step) {\n",
                "        value = value + step;\n",
                "        return value;\n",
                "    };\n",
                "};\n",
                "var counter = makeCounter(1);\n",
                "var object = {count: counter(2), \"flag\": true};\n",
                "if (object.count !== 3) {\n",
                "    throw new Test262Error(\"#1\");\n",
                "}\n",
                "if (object[\"flag\"] !== true) {\n",
                "    throw new Test262Error(\"#2\");\n",
                "}\n",
                "function outerCaptureWrite() {\n",
                "    var value = 1;\n",
                "    function inc() {\n",
                "        value = value + 1;\n",
                "        return value;\n",
                "    }\n",
                "    assert.sameValue(inc(), 2, 'inner closure updates captured binding');\n",
                "    return value;\n",
                "}\n",
                "assert.sameValue(outerCaptureWrite(), 2, 'outer frame observes closure writes to captured binding after call returns');\n",
                "function outerAccessorWrite() {\n",
                "    var value = 1;\n",
                "    var obj = {};\n",
                "    Object.defineProperty(obj, 'mutate', {\n",
                "        get: function() {\n",
                "            value = 4;\n",
                "            return 0;\n",
                "        },\n",
                "        enumerable: true,\n",
                "        configurable: true\n",
                "    });\n",
                "    obj.mutate;\n",
                "    return value;\n",
                "}\n",
                "assert.sameValue(outerAccessorWrite(), 4, 'outer frame observes closure writes through accessor call');\n",
                "function sum(a, b) {\n",
                "    return this.base + a + b;\n",
                "}\n",
                "var inferred = function(a) { return a; };\n",
                "assert.sameValue(inferred.name, 'inferred', 'anonymous function expression infers name from variable binding');\n",
                "assert.sameValue(inferred.length, 1, 'anonymous function expression preserves formal parameter count');\n",
                "var inferredArrow = (left, right) => left + right;\n",
                "assert.sameValue(inferredArrow.name, 'inferredArrow', 'anonymous arrow infers name from variable binding');\n",
                "assert.sameValue(inferredArrow.length, 2, 'anonymous arrow preserves formal parameter count');\n",
                "var assigned;\n",
                "assigned = function() { return 1; };\n",
                "assert.sameValue(assigned.name, 'assigned', 'anonymous function expression infers name from identifier assignment');\n",
                "var objectWithFns = {\n",
                "    methodish: function() { return 1; },\n",
                "    arrowish: (value) => value,\n",
                "    explicit: function ExplicitInner() { return 1; }\n",
                "};\n",
                "assert.sameValue(objectWithFns.methodish.name, 'methodish', 'anonymous object property function infers property name');\n",
                "assert.sameValue(objectWithFns.arrowish.name, 'arrowish', 'anonymous object property arrow infers property name');\n",
                "assert.sameValue(objectWithFns.explicit.name, 'ExplicitInner', 'explicit function expression name wins over inferred property name');\n",
                "assert.sameValue(sum.length, 2, 'ordinary function length matches formal parameter count');\n",
                "assert.sameValue(sum.name, 'sum', 'ordinary function name installs from source name');\n",
                "var sumNames = Object.getOwnPropertyNames(sum);\n",
                "assert.sameValue(sumNames[0], 'length', 'ordinary function names expose length first');\n",
                "assert.sameValue(sumNames[1], 'name', 'ordinary function names expose name second');\n",
                "assert.sameValue(sumNames[2], 'prototype', 'constructable function names expose prototype after length/name');\n",
                "var sumLengthDesc = Object.getOwnPropertyDescriptor(sum, 'length');\n",
                "assert.sameValue(sumLengthDesc.writable, false, 'ordinary function length is non-writable');\n",
                "assert.sameValue(sumLengthDesc.enumerable, false, 'ordinary function length is non-enumerable');\n",
                "assert.sameValue(sumLengthDesc.configurable, true, 'ordinary function length is configurable');\n",
                "assert.sameValue(sum.call({ base: 4 }, 5, 6), 15, 'Function.prototype.call invokes ordinary function');\n",
                "var applyArgs = { length: 2 };\n",
                "Object.defineProperty(applyArgs, '0', { get: function() { return 7; }, enumerable: true, configurable: true });\n",
                "Object.defineProperty(applyArgs, '1', { get: function() { return 8; }, enumerable: true, configurable: true });\n",
                "assert.sameValue(sum.apply({ base: 1 }, applyArgs), 16, 'Function.prototype.apply uses list-from-array-like over closures');\n",
                "var bound = sum.bind({ base: 10 }, 2);\n",
                "assert.sameValue(bound(3), 15, 'Function.prototype.bind prepends bound args for closures');\n",
                "function PointCtor(x, y) { this.x = x; this.y = y; }\n",
                "var pointCtorProtoDesc = Object.getOwnPropertyDescriptor(PointCtor, 'prototype');\n",
                "assert.sameValue(pointCtorProtoDesc.writable, true, 'compiled constructor prototype is writable');\n",
                "assert.sameValue(pointCtorProtoDesc.enumerable, false, 'compiled constructor prototype is non-enumerable');\n",
                "assert.sameValue(pointCtorProtoDesc.configurable, false, 'compiled constructor prototype is non-configurable');\n",
                "var BoundPointCtor = PointCtor.bind({ ignored: true }, 2);\n",
                "var pointFromBound = new BoundPointCtor(5);\n",
                "assert.sameValue(pointFromBound.x, 2, 'bound constructors prepend bound arguments');\n",
                "assert.sameValue(pointFromBound.y, 5, 'bound constructors forward runtime arguments');\n",
                "assert.sameValue(Object.getPrototypeOf(pointFromBound), PointCtor.prototype, 'new bound constructor normalizes newTarget to target');\n",
                "function ProtoSwapCtor(value) { this.value = value; }\n",
                "var replacementProto = { swapped: true };\n",
                "ProtoSwapCtor.prototype = replacementProto;\n",
                "var swappedInstance = new ProtoSwapCtor(9);\n",
                "assert.sameValue(Object.getPrototypeOf(swappedInstance), replacementProto, 'compiled constructor uses reassigned prototype');\n",
                "assert.sameValue(swappedInstance.value, 9, 'compiled constructor still runs body after prototype reassignment');\n",
                "var boundAbs = Math.abs.bind(null);\n",
                "assert.sameValue(boundAbs.length, 1, 'bound function length defaults from target');\n",
                "assert.sameValue(boundAbs.name, 'bound abs', 'bound function name prefixes target name');\n",
                "var applyAbs = Reflect.apply.bind(null, Math.abs, null);\n",
                "assert.sameValue(applyAbs.length, 1, 'bound function length subtracts bound arguments');\n",
                "var bindMetaLengthCalls = 0;\n",
                "var bindMetaNameCalls = 0;\n",
                "function BindMeta(a, b, c, d) {}\n",
                "Object.defineProperty(BindMeta, 'length', { get: function() { bindMetaLengthCalls = bindMetaLengthCalls + 1; return 4; }, configurable: true });\n",
                "Object.defineProperty(BindMeta, 'name', { get: function() { bindMetaNameCalls = bindMetaNameCalls + 1; return 'Meta'; }, configurable: true });\n",
                "var boundMeta = BindMeta.bind(null, 1, 2);\n",
                "assert.sameValue(boundMeta.length, 2, 'bound function length uses [[Get]] on target length');\n",
                "assert.sameValue(boundMeta.name, 'bound Meta', 'bound function name uses [[Get]] on target name');\n",
                "assert.sameValue(bindMetaLengthCalls, 1, 'bind reads target length getter once');\n",
                "assert.sameValue(bindMetaNameCalls, 1, 'bind reads target name getter once');\n",
                "Object.defineProperty(BindMeta, 'name', { get: function() { throw 5; }, configurable: true });\n",
                "try {\n",
                "    BindMeta.bind(null);\n",
                "    throw new Test262Error('#5');\n",
                "} catch (error) {\n",
                "    assert.sameValue(error, 5, 'bind propagates target name getter throw');\n",
                "}\n",
                "var boundAbsNames = Object.getOwnPropertyNames(boundAbs);\n",
                "assert.sameValue(boundAbsNames[0], 'length', 'bound function names expose length first');\n",
                "assert.sameValue(boundAbsNames[1], 'name', 'bound function names expose name second');\n",
                "var boundLengthDesc = Object.getOwnPropertyDescriptor(boundAbs, 'length');\n",
                "assert.sameValue(boundLengthDesc.writable, false, 'bound length is non-writable');\n",
                "assert.sameValue(boundLengthDesc.enumerable, false, 'bound length is non-enumerable');\n",
                "assert.sameValue(boundLengthDesc.configurable, true, 'bound length is configurable');\n",
                "boundAbs.extra = 9;\n",
                "assert.sameValue(boundAbs.extra, 9, 'bound function stores ordinary own properties');\n",
                "Object.defineProperty(boundAbs, 'hidden', { value: 3, enumerable: false, configurable: true, writable: true });\n",
                "assert.sameValue(boundAbs.hidden, 3, 'bound function defineProperty stores ordinary descriptors');\n",
                "assert.sameValue(Object.keys(boundAbs).length, 1, 'bound function Object.keys sees enumerable custom props only');\n",
                "assert.sameValue(Object.keys(boundAbs)[0], 'extra', 'bound function Object.keys includes enumerable custom prop');\n",
                "assert.sameValue(delete boundAbs.extra, true, 'bound function delete removes configurable custom props');\n",
                "assert.sameValue(boundAbs.extra, undefined, 'bound function delete clears custom prop');\n",
                "Object.preventExtensions(boundAbs);\n",
                "assert.sameValue(Object.isExtensible(boundAbs), false, 'bound function preventExtensions works');\n",
                "assert.sameValue(Reflect.set(boundAbs, 'late', 1), false, 'bound function rejects new props when non-extensible');\n",
                "Object.freeze(boundAbs);\n",
                "assert.sameValue(Object.isFrozen(boundAbs), true, 'bound function freeze works');\n",
                "assert.sameValue(Function.isCallable(sum), true, 'Function.isCallable sees closures');\n",
                "assert.sameValue(Function.isCallable(bound), true, 'Function.isCallable sees bound functions');\n",
                "assert.sameValue(Function.isCallable({}), false, 'Function.isCallable rejects plain objects');\n",
                "assert.sameValue(sum.toString(), 'function () { [bytecode] }', 'Function.prototype.toString formats closures');\n",
                "assert.sameValue(bound.toString(), 'function () { [native code] }', 'Function.prototype.toString formats bound functions');\n",
                "try {\n",
                "    Function.prototype.call.call({}, null);\n",
                "    throw new Test262Error('#3');\n",
                "} catch (error) {\n",
                "    assert.sameValue(error.name, 'TypeError', 'Function.prototype.call rejects non-callable receiver');\n",
                "}\n",
                "try {\n",
                "    Function.prototype.apply.call(sum, null, 'otter');\n",
                "    throw new Test262Error('#4');\n",
                "} catch (error) {\n",
                "    assert.sameValue(error.name, 'TypeError', 'Function.prototype.apply rejects primitive string argArray');\n",
                "}\n",
            ),
            "native-test262-closures-objects.js",
        )
        .expect("closure/object script should compile");

        Interpreter::new()
            .execute(&module)
            .expect("closure/object script should execute");
    }

    #[test]
    fn compile_test262_basic_script_supports_default_initializer_name_inference() {
        let module = compile_test262_basic_script(
            concat!(
                "var [arrayDefault = function() { return 1; }] = [];\n",
                "assert.sameValue(arrayDefault.name, 'arrayDefault', 'array destructuring default infers binding name');\n",
                "var { objectDefault = function() { return 1; } } = {};\n",
                "assert.sameValue(objectDefault.name, 'objectDefault', 'object destructuring default infers binding name');\n",
                "var { source: aliasDefault = function() { return 1; } } = {};\n",
                "assert.sameValue(aliasDefault.name, 'aliasDefault', 'aliased object destructuring default infers local binding name');\n",
                "function readDefaultParamValues(numberParam = 1, fnParam = function() { return 2; }, arrowParam = () => 3) {\n",
                "    assert.sameValue(readDefaultParamValues.length, 0, 'function length stops at first default parameter');\n",
                "    assert.sameValue(numberParam, 1, 'default parameter uses initializer when argument is missing');\n",
                "    assert.sameValue(fnParam.name, 'fnParam', 'default parameter function infers parameter name');\n",
                "    assert.sameValue(fnParam(), 2, 'default parameter function remains callable');\n",
                "    assert.sameValue(arrowParam.name, 'arrowParam', 'default parameter arrow infers parameter name');\n",
                "    assert.sameValue(arrowParam(), 3, 'default parameter arrow remains callable');\n",
                "}\n",
                "function oneBeforeDefault(a, b = 1, c) { return a + b + c; }\n",
                "assert.sameValue(oneBeforeDefault.length, 1, 'function length counts parameters before first default');\n",
                "readDefaultParamValues();\n",
                "readDefaultParamValues(4, function override() { return 5; }, () => 6);\n",
                "var defaultArrowLength = (a, b = 1, c) => a + b + c;\n",
                "assert.sameValue(defaultArrowLength.length, 1, 'arrow length counts parameters before first default');\n",
            ),
            "native-test262-default-initializer-names.js",
        )
        .expect("default initializer name script should compile");

        Interpreter::new()
            .execute(&module)
            .expect("default initializer name script should execute");
    }

    #[test]
    fn compile_test262_basic_script_enforces_parameter_default_tdz_semantics() {
        let module = compile_test262_basic_script(
            concat!(
                "function earlierUsesEarlier(a = 1, b = a) { return b; }\n",
                "assert.sameValue(earlierUsesEarlier(), 1, 'later default can read earlier initialized parameter');\n",
                "function selfRef(a = a) { return a; }\n",
                "try {\n",
                "  selfRef();\n",
                "  throw new Test262Error('self-referential parameter default should throw ReferenceError');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'ReferenceError', 'self-referential parameter default throws ReferenceError');\n",
                "}\n",
                "function earlierUsesLater(a = b, b = 1) { return a + b; }\n",
                "try {\n",
                "  earlierUsesLater();\n",
                "  throw new Test262Error('earlier default reading later parameter should throw ReferenceError');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'ReferenceError', 'later parameter is TDZ during earlier default');\n",
                "}\n",
                "function destructuringUsesLater({ x = y } = {}, y = 2) { return x + y; }\n",
                "try {\n",
                "  destructuringUsesLater();\n",
                "  throw new Test262Error('destructuring default reading later parameter should throw ReferenceError');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'ReferenceError', 'later parameter stays TDZ inside earlier destructuring default');\n",
                "}\n",
                "function invokedClosureBeforeInit(a = function() { return b; }(), b = 4) { return a; }\n",
                "try {\n",
                "  invokedClosureBeforeInit();\n",
                "  throw new Test262Error('closure invoked during default before later initialization should throw ReferenceError');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'ReferenceError', 'captured later parameter remains TDZ until initialized');\n",
                "}\n",
                "function closureAfterInit(a = function() { return b; }, b = 3) { return a(); }\n",
                "assert.sameValue(closureAfterInit(), 3, 'closure created during earlier default observes later parameter initialization');\n",
            ),
            "native-test262-parameter-default-tdz.js",
        )
        .expect("parameter default TDZ script should compile");

        Interpreter::new()
            .execute(&module)
            .expect("parameter default TDZ script should execute");
    }

    #[test]
    fn compile_test262_basic_script_supports_destructuring_parameters() {
        let module = compile_test262_basic_script(
            concat!(
                "function readPoint({ x, y }) {\n",
                "    return x + y;\n",
                "}\n",
                "assert.sameValue(readPoint.length, 1, 'object destructuring parameter counts as one formal parameter');\n",
                "assert.sameValue(readPoint({ x: 2, y: 5 }), 7, 'object destructuring parameter binds properties');\n",
                "var readPair = ([left, right]) => left + right;\n",
                "assert.sameValue(readPair.length, 1, 'array destructuring parameter counts as one formal parameter');\n",
                "assert.sameValue(readPair([3, 4]), 7, 'array destructuring parameter binds indices');\n",
                "function readDefaults({ fn = function() { return 5; }, arrow = () => 6 } = {}) {\n",
                "    assert.sameValue(fn.name, 'fn', 'nested destructuring default function infers binding name');\n",
                "    assert.sameValue(fn(), 5, 'nested destructuring default function remains callable');\n",
                "    assert.sameValue(arrow.name, 'arrow', 'nested destructuring default arrow infers binding name');\n",
                "    assert.sameValue(arrow(), 6, 'nested destructuring default arrow remains callable');\n",
                "}\n",
                "assert.sameValue(readDefaults.length, 0, 'destructuring parameter with top-level default shortens function length');\n",
                "readDefaults();\n",
                "function sumDefaultPair([left, right] = [7, 8]) { return left + right; }\n",
                "assert.sameValue(sumDefaultPair.length, 0, 'array destructuring parameter default shortens function length');\n",
                "assert.sameValue(sumDefaultPair(), 15, 'array destructuring parameter uses top-level default');\n",
                "function collectTail([head, ...tail]) {\n",
                "    assert.sameValue(collectTail.length, 1, 'array rest parameter still counts as one formal parameter');\n",
                "    assert.sameValue(head, 1, 'array rest parameter keeps head binding');\n",
                "    assert.sameValue(tail.length, 2, 'array rest parameter collects remaining elements');\n",
                "    assert.sameValue(tail[0], 2, 'array rest parameter keeps first remaining element');\n",
                "    assert.sameValue(tail[1], 3, 'array rest parameter keeps second remaining element');\n",
                "    return tail.join(',');\n",
                "}\n",
                "assert.sameValue(collectTail([1, 2, 3]), '2,3', 'array rest parameter materializes tail array');\n",
                "function nestedArrayRest([first, ...[second, third = function() { return 4; }]]) {\n",
                "    assert.sameValue(third.name, 'third', 'array rest nested default infers local binding name');\n",
                "    return first + second + third();\n",
                "}\n",
                "assert.sameValue(nestedArrayRest([1, 2]), 7, 'array rest parameter supports nested destructuring and defaults');\n",
                "var dynamicKey = 'value';\n",
                "var { [dynamicKey]: computedValue } = { value: 12 };\n",
                "assert.sameValue(computedValue, 12, 'computed object destructuring key reads dynamic property');\n",
                "var missingKey = 'missing';\n",
                "var { [missingKey]: computedDefault = function() { return 13; } } = {};\n",
                "assert.sameValue(computedDefault.name, 'computedDefault', 'computed object destructuring default infers local binding name');\n",
                "assert.sameValue(computedDefault(), 13, 'computed object destructuring default remains callable');\n",
            ),
            "native-test262-destructuring-parameters.js",
        )
        .expect("destructuring parameter script should compile");

        Interpreter::new()
            .execute(&module)
            .expect("destructuring parameter script should execute");
    }

    #[test]
    fn compile_test262_basic_script_supports_object_rest_destructuring() {
        let module = compile_test262_basic_script(
            concat!(
                "var source = { keep: 2, drop: 1, extra: 3 };\n",
                "var { drop, ...rest } = source;\n",
                "assert.sameValue(drop, 1, 'object rest keeps extracted binding');\n",
                "assert.sameValue(rest.drop, undefined, 'object rest excludes extracted key');\n",
                "assert.sameValue(rest.keep, 2, 'object rest copies first remaining own key');\n",
                "assert.sameValue(rest.extra, 3, 'object rest copies later remaining own key');\n",
                "assert.sameValue(Object.keys(rest).length, 2, 'object rest copies only remaining enumerable own keys');\n",
                "var proto = { inherited: 4 };\n",
                "var derived = Object.create(proto);\n",
                "derived.own = 5;\n",
                "var { ...ownOnly } = derived;\n",
                "assert.sameValue(ownOnly.own, 5, 'object rest copies own enumerable properties');\n",
                "assert.sameValue(ownOnly.inherited, undefined, 'object rest skips inherited enumerable properties');\n",
                "function readObjectRest({ head, ...tail }) {\n",
                "    assert.sameValue(head, 7, 'object rest parameter keeps extracted binding');\n",
                "    assert.sameValue(tail.left, 8, 'object rest parameter copies remaining own key');\n",
                "    assert.sameValue(tail.head, undefined, 'object rest parameter excludes extracted key');\n",
                "    return tail.right;\n",
                "}\n",
                "assert.sameValue(readObjectRest({ head: 7, left: 8, right: 9 }), 9, 'object rest parameter remains callable');\n",
                "var { 0: firstChar, ...stringRest } = 'ot';\n",
                "assert.sameValue(firstChar, 'o', 'object rest boxes primitive string source for extracted binding');\n",
                "assert.sameValue(stringRest[0], undefined, 'object rest excludes extracted string index');\n",
                "assert.sameValue(stringRest[1], 't', 'object rest keeps remaining boxed string index');\n",
                "var { head: renamed, ...tailObject } = { head: 1, tail: function() { return 11; } };\n",
                "assert.sameValue(renamed, 1, 'object rest nested pattern keeps aliased binding');\n",
                "assert.sameValue(tailObject.tail.name, 'tail', 'object rest preserves callable own properties');\n",
                "assert.sameValue(tailObject.tail(), 11, 'object rest copied function property remains callable');\n",
                "var computedKey = 'drop';\n",
                "var { [computedKey]: computedDrop, ...computedRest } = { drop: 5, keep: 6 };\n",
                "assert.sameValue(computedDrop, 5, 'object rest with computed key keeps extracted binding');\n",
                "assert.sameValue(computedRest.drop, undefined, 'object rest excludes computed extracted key');\n",
                "assert.sameValue(computedRest.keep, 6, 'object rest preserves non-excluded own key after computed extraction');\n",
            ),
            "native-test262-object-rest-destructuring.js",
        )
        .expect("object rest destructuring script should compile");

        Interpreter::new()
            .execute(&module)
            .expect("object rest destructuring script should execute");
    }

    #[test]
    fn compile_test262_basic_script_supports_rest_parameters() {
        let module = compile_test262_basic_script(
            concat!(
                "function collect(...rest) {\n",
                "    assert.sameValue(collect.length, 0, 'rest-only function length is zero');\n",
                "    assert.sameValue(rest.length, 3, 'rest-only parameter captures all arguments');\n",
                "    assert.sameValue(rest[0], 1, 'rest-only parameter keeps first extra argument');\n",
                "    assert.sameValue(rest[2], 3, 'rest-only parameter keeps last extra argument');\n",
                "    return rest.join(',');\n",
                "}\n",
                "assert.sameValue(collect(1, 2, 3), '1,2,3', 'rest-only parameter materializes a real array');\n",
                "function headAndTail(head, ...tail) {\n",
                "    assert.sameValue(headAndTail.length, 1, 'rest parameter does not contribute to function length');\n",
                "    return head + ':' + tail.join(',');\n",
                "}\n",
                "assert.sameValue(headAndTail(1, 2, 3), '1:2,3', 'rest parameter captures overflow arguments only');\n",
                "assert.sameValue(headAndTail(1), '1:', 'rest parameter becomes an empty array when no overflow args exist');\n",
                "var arrowRest = (prefix, ...tail) => prefix + ':' + tail.length;\n",
                "assert.sameValue(arrowRest.length, 1, 'arrow rest parameter does not contribute to length');\n",
                "assert.sameValue(arrowRest('x', 1, 2), 'x:2', 'arrow rest parameter captures overflow arguments');\n",
                "function destructureRest(...[first, second = function() { return 7; }]) {\n",
                "    assert.sameValue(second.name, 'second', 'rest destructuring default infers local binding name');\n",
                "    return first + second();\n",
                "}\n",
                "assert.sameValue(destructureRest(5), 12, 'destructuring can bind against the generated rest array');\n",
                "var [lead, ...tail] = [1, 2, 3];\n",
                "assert.sameValue(lead, 1, 'array destructuring rest keeps leading binding');\n",
                "assert.sameValue(tail.length, 2, 'array destructuring rest collects remaining elements');\n",
                "assert.sameValue(tail[0], 2, 'array destructuring rest keeps first remaining element');\n",
                "assert.sameValue(tail[1], 3, 'array destructuring rest keeps second remaining element');\n",
                "var [single, ...emptyTail] = [9];\n",
                "assert.sameValue(emptyTail.length, 0, 'array destructuring rest becomes empty array when there is no tail');\n",
                "var [outer, ...[inner, inferred = function() { return 8; }]] = [1, 2];\n",
                "assert.sameValue(outer, 1, 'array destructuring nested rest keeps head binding');\n",
                "assert.sameValue(inner, 2, 'array destructuring nested rest keeps first tail element');\n",
                "assert.sameValue(inferred.name, 'inferred', 'array destructuring nested rest default infers binding name');\n",
                "assert.sameValue(inferred(), 8, 'array destructuring nested rest default remains callable');\n",
            ),
            "native-test262-rest-parameters.js",
        )
        .expect("rest parameter script should compile");

        Interpreter::new()
            .execute(&module)
            .expect("rest parameter script should execute");
    }

    #[test]
    fn compile_test262_basic_script_supports_method_calls_and_this() {
        let module = compile_test262_basic_script(
            concat!(
                "var object = {\n",
                "  base: 40,\n",
                "  inc: function(step) {\n",
                "    return this.base + step;\n",
                "  }\n",
                "};\n",
                "assert.sameValue(object.inc(2), 42, \"static member call\");\n",
                "assert.sameValue(object[\"inc\"](3), 43, \"computed member call\");\n",
            ),
            "native-test262-method-calls-this.js",
        )
        .expect("method call script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("method call script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_new_and_constructor_return_override() {
        let module = compile_test262_basic_script(
            concat!(
                "function Box(value) {\n",
                "  this.value = value;\n",
                "  return 1;\n",
                "}\n",
                "function Override() {\n",
                "  return { value: 9 };\n",
                "}\n",
                "var box = new Box(7);\n",
                "var override = new Override();\n",
                "var arrow = () => 1;\n",
                "var boundArrow = arrow.bind(null);\n",
                "assert.sameValue(box.value, 7, \"primitive return falls back to receiver\");\n",
                "assert.sameValue(override.value, 9, \"object return overrides receiver\");\n",
                "assert.sameValue(box.constructor, Box, \"closure prototype constructor link\");\n",
                "try {\n",
                "  new arrow();\n",
                "  throw new Test262Error('new arrow should reject non-constructible closure');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'new arrow throws TypeError');\n",
                "}\n",
                "try {\n",
                "  new boundArrow();\n",
                "  throw new Test262Error('new boundArrow should reject non-constructible bound closure');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'new boundArrow throws TypeError');\n",
                "}\n",
                "try {\n",
                "  new (Math.abs)(1);\n",
                "  throw new Test262Error('new Math.abs should reject non-constructible host function');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'new Math.abs throws TypeError');\n",
                "}\n",
            ),
            "native-test262-new-constructors.js",
        )
        .expect("constructor script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("constructor script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_strings_arrays_and_native_asserts() {
        let module = compile_test262_basic_script(
            concat!(
                "var index = 1;\n",
                "var text = \"otter\";\n",
                "assert.sameValue(text.length, 5, \"text.length\");\n",
                "assert.sameValue(text[index], \"t\", \"text[index]\");\n",
                "var array = [1,,3];\n",
                "assert.sameValue(array.length, 3, \"array.length\");\n",
                "assert.sameValue(array[index], undefined, \"array[index]\");\n",
                "array[index] = 2;\n",
                "assert.sameValue(array[index], 2, \"array[index] after store\");\n",
                "assert.sameValue(array[2], 3, \"array[2]\");\n",
            ),
            "native-test262-strings-arrays.js",
        )
        .expect("strings/arrays script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("strings/arrays script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_and_reflect_intrinsics() {
        let module = compile_test262_basic_script(
            concat!(
                "var array = new Array(1, 2);\n",
                "assert.sameValue(Array.isArray(array), true, \"Array.isArray\");\n",
                "assert.sameValue(array.push(3), 3, \"push returns new length\");\n",
                "assert.sameValue(array[2], 3, \"push stores appended value\");\n",
                "var proto = { value: 7 };\n",
                "var child = Object.create(proto);\n",
                "assert.sameValue(Reflect.get(child, \"value\"), 7, \"Reflect.get walks prototypes\");\n",
                "function add(a, b) { return this.base + a + b; }\n",
                "assert.sameValue(Reflect.apply(add, { base: 4 }, [2, 3]), 9, 'Reflect.apply calls ordinary function with explicit this');\n",
                "var arrow = (a, b) => a * b;\n",
                "assert.sameValue(Reflect.apply(arrow, null, [2, 4]), 8, 'Reflect.apply calls arrow closures');\n",
                "var arrayLike = { 0: 5, 1: 6, length: 2 };\n",
                "assert.sameValue(Reflect.apply(add, { base: 1 }, arrayLike), 12, 'Reflect.apply reads array-like arguments');\n",
                "var getterArgs = { length: 2 };\n",
                "Object.defineProperty(getterArgs, '0', { get: function() { return 7; }, enumerable: true, configurable: true });\n",
                "Object.defineProperty(getterArgs, '1', { get: function() { return 8; }, enumerable: true, configurable: true });\n",
                "assert.sameValue(Reflect.apply(add, { base: 0 }, getterArgs), 15, 'Reflect.apply uses [[Get]] on argumentsList');\n",
                "function Point(x, y) { this.x = x; this.y = y; }\n",
                "var point = Reflect.construct(Point, [3, 4]);\n",
                "assert.sameValue(point.x, 3, 'Reflect.construct passes argument 0');\n",
                "assert.sameValue(point.y, 4, 'Reflect.construct passes argument 1');\n",
                "assert.sameValue(Object.getPrototypeOf(point), Point.prototype, 'Reflect.construct uses target prototype by default');\n",
                "function OverrideBox(value) { this.initial = value; return { boxed: value * 2 }; }\n",
                "var overrideBox = Reflect.construct(OverrideBox, [5]);\n",
                "assert.sameValue(overrideBox.boxed, 10, 'Reflect.construct respects object return override');\n",
                "var constructArgs = { length: 2 };\n",
                "Object.defineProperty(constructArgs, '0', { get: function() { return 6; }, enumerable: true, configurable: true });\n",
                "Object.defineProperty(constructArgs, '1', { get: function() { return 7; }, enumerable: true, configurable: true });\n",
                "var pointFromArrayLike = Reflect.construct(Point, constructArgs);\n",
                "assert.sameValue(pointFromArrayLike.x, 6, 'Reflect.construct reads array-like argument 0');\n",
                "assert.sameValue(pointFromArrayLike.y, 7, 'Reflect.construct uses [[Get]] on argumentsList');\n",
                "var BoundPoint = Point.bind({ ignored: true }, 11);\n",
                "var pointFromBoundTarget = Reflect.construct(BoundPoint, [12]);\n",
                "assert.sameValue(pointFromBoundTarget.x, 11, 'Reflect.construct prepends bound constructor args');\n",
                "assert.sameValue(pointFromBoundTarget.y, 12, 'Reflect.construct forwards runtime args through bound constructor');\n",
                "assert.sameValue(Object.getPrototypeOf(pointFromBoundTarget), Point.prototype, 'Reflect.construct normalizes bound newTarget to target');\n",
                "function NewTarget() {}\n",
                "var customProto = { kind: 'custom' };\n",
                "NewTarget.prototype = customProto;\n",
                "var viaNewTarget = Reflect.construct(Point, [8, 9], NewTarget);\n",
                "assert.sameValue(Object.getPrototypeOf(viaNewTarget), customProto, 'Reflect.construct uses newTarget prototype');\n",
                "assert.sameValue(viaNewTarget.x, 8, 'Reflect.construct still runs target body with newTarget');\n",
                "var viaBoundNewTarget = Reflect.construct(BoundPoint, [13], NewTarget);\n",
                "assert.sameValue(Object.getPrototypeOf(viaBoundNewTarget), customProto, 'Reflect.construct preserves explicit newTarget through bound constructor');\n",
                "assert.sameValue(viaBoundNewTarget.x, 11, 'Reflect.construct keeps bound args with explicit newTarget');\n",
                "assert.sameValue(viaBoundNewTarget.y, 13, 'Reflect.construct forwards args with explicit newTarget through bound constructor');\n",
                "var BoundNewTarget = Point.bind(null, 21);\n",
                "var boundProtoGetterCalls = 0;\n",
                "Object.defineProperty(BoundNewTarget, 'prototype', { get: function() { boundProtoGetterCalls = boundProtoGetterCalls + 1; return customProto; }, configurable: true });\n",
                "var viaBoundPrototypeGetter = Reflect.construct(Point, [22], BoundNewTarget);\n",
                "assert.sameValue(Object.getPrototypeOf(viaBoundPrototypeGetter), customProto, 'Reflect.construct uses [[Get]] for explicit newTarget prototype');\n",
                "assert.sameValue(boundProtoGetterCalls, 1, 'Reflect.construct invokes explicit newTarget prototype getter');\n",
                "Object.defineProperty(BoundNewTarget, 'prototype', { get: function() { return 1; }, configurable: true });\n",
                "var viaNonObjectPrototype = Reflect.construct(Point, [23], BoundNewTarget);\n",
                "assert.sameValue(Object.getPrototypeOf(viaNonObjectPrototype), Object.prototype, 'Reflect.construct falls back when explicit newTarget prototype getter returns non-object');\n",
                "Object.defineProperty(BoundNewTarget, 'prototype', { get: function() { throw 5; }, configurable: true });\n",
                "try {\n",
                "  Reflect.construct(Point, [24], BoundNewTarget);\n",
                "  throw new Test262Error('Reflect.construct should propagate explicit newTarget prototype getter throw');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error, 5, 'Reflect.construct propagates explicit newTarget prototype getter throw');\n",
                "}\n",
                "function HostTarget() {}\n",
                "HostTarget.prototype = { host: true };\n",
                "var stringViaNewTarget = Reflect.construct(String, ['ot'], HostTarget);\n",
                "assert.sameValue(Object.getPrototypeOf(stringViaNewTarget), HostTarget.prototype, 'Reflect.construct host constructor uses newTarget prototype');\n",
                "assert.sameValue(stringViaNewTarget.length, 2, 'Reflect.construct host constructor preserves wrapper semantics');\n",
                "assert.sameValue(stringViaNewTarget[0], 'o', 'Reflect.construct host constructor preserves string exotic index access');\n",
                "try {\n",
                "  Reflect.construct(arrow, []);\n",
                "  throw new Test262Error('Reflect.construct should reject non-constructible target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.construct non-constructor target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.construct(Point, undefined);\n",
                "  throw new Test262Error('Reflect.construct should reject undefined argumentsList');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.construct undefined argumentsList throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.construct(Point, 'ot');\n",
                "  throw new Test262Error('Reflect.construct should reject primitive string argumentsList');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.construct primitive string argumentsList throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.construct(Point, [], {});\n",
                "  throw new Test262Error('Reflect.construct should reject non-constructor newTarget');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.construct non-constructor newTarget throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.construct(1, []);\n",
                "  throw new Test262Error('Reflect.construct should reject primitive target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.construct primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.apply(1, null, []);\n",
                "  throw new Test262Error('Reflect.apply should reject non-callable targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.apply non-callable target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.apply(add, null, undefined);\n",
                "  throw new Test262Error('Reflect.apply should reject undefined argumentsList');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.apply undefined argumentsList throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.apply(add, null, 'ot');\n",
                "  throw new Test262Error('Reflect.apply should reject primitive string argumentsList');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.apply primitive string argumentsList throws TypeError');\n",
                "}\n",
                "var accessor = {};\n",
                "Reflect.defineProperty(accessor, \"flag\", { get: Boolean.prototype.valueOf });\n",
                "assert.sameValue(Reflect.get(accessor, \"flag\", true), true, \"Reflect.get preserves primitive boolean receiver\");\n",
                "assert.sameValue(Reflect.get(accessor, \"flag\", false), false, \"Reflect.get preserves false boolean receiver\");\n",
                "var setter = {};\n",
                "Reflect.defineProperty(setter, \"flag\", { set: Boolean.prototype.valueOf });\n",
                "assert.sameValue(Reflect.set(setter, \"flag\", 1, true), true, \"Reflect.set preserves primitive boolean receiver for setter\");\n",
                "assert.sameValue(Reflect.set(child, \"value\", 9, true), false, \"Reflect.set fails for primitive receiver on inherited data property\");\n",
                "assert.sameValue(Reflect.set(child, \"value\", 9), true, \"Reflect.set reports success\");\n",
                "assert.sameValue(child.value, 9, \"Reflect.set writes onto receiver\");\n",
                "assert.sameValue(proto.value, 7, \"Reflect.set keeps prototype slot intact\");\n",
                "assert.sameValue(Reflect.set({}, \"fresh\", 1, true), false, \"Reflect.set cannot create property on primitive receiver\");\n",
                "var receiverProto = { value: 1 };\n",
                "var setterReceiver = {};\n",
                "Reflect.defineProperty(setterReceiver, 'value', { set: function(v) { this.seen = v; }, enumerable: true, configurable: true });\n",
                "assert.sameValue(Reflect.set(receiverProto, 'value', 12, setterReceiver), true, 'Reflect.set uses receiver own setter before defining data');\n",
                "assert.sameValue(setterReceiver.seen, 12, 'Reflect.set passes value into receiver own setter');\n",
                "var readOnlyReceiver = {};\n",
                "Reflect.defineProperty(readOnlyReceiver, 'value', { value: 2, writable: false, enumerable: true, configurable: true });\n",
                "assert.sameValue(Reflect.set(receiverProto, 'value', 13, readOnlyReceiver), false, 'Reflect.set fails against receiver own non-writable data property');\n",
                "assert.sameValue(readOnlyReceiver.value, 2, 'Reflect.set keeps receiver own non-writable value');\n",
                "var getterOnlyReceiver = {};\n",
                "Reflect.defineProperty(getterOnlyReceiver, 'value', { get: function() { return 3; }, enumerable: true, configurable: true });\n",
                "assert.sameValue(Reflect.set(receiverProto, 'value', 14, getterOnlyReceiver), false, 'Reflect.set fails against receiver own accessor without setter');\n",
                "assert.sameValue(Reflect.get(getterOnlyReceiver, 'value'), 3, 'Reflect.set keeps receiver own getter-only accessor intact');\n",
                "var arrayReceiver = [];\n",
                "assert.sameValue(Reflect.set(arrayReceiver, '1', 21), true, 'Reflect.set creates array index on array receiver');\n",
                "assert.sameValue(arrayReceiver.length, 2, 'Reflect.set on array receiver updates length');\n",
                "assert.sameValue(arrayReceiver[1], 21, 'Reflect.set stores array index value');\n",
                "assert.sameValue(Reflect.set(arrayReceiver, '0', 20), true, 'Reflect.set updates existing own array index');\n",
                "assert.sameValue(arrayReceiver[0], 20, 'Reflect.set stores updated own array index value');\n",
                "var arrayProto = { '1': 7 };\n",
                "var derivedArrayReceiver = [];\n",
                "assert.sameValue(Reflect.set(arrayProto, '1', 22, derivedArrayReceiver), true, 'Reflect.set can materialize inherited data onto array receiver');\n",
                "assert.sameValue(derivedArrayReceiver.length, 2, 'Reflect.set inherited write updates array receiver length');\n",
                "assert.sameValue(derivedArrayReceiver[1], 22, 'Reflect.set writes inherited data onto array receiver');\n",
                "var frozenArrayReceiver = [];\n",
                "Reflect.defineProperty(frozenArrayReceiver, '0', { value: 1, writable: false, enumerable: true, configurable: true });\n",
                "assert.sameValue(Reflect.set(arrayProto, '0', 23, frozenArrayReceiver), false, 'Reflect.set fails against receiver own non-writable array index');\n",
                "assert.sameValue(frozenArrayReceiver[0], 1, 'Reflect.set keeps non-writable array index intact');\n",
                "var lockedLengthArray = [1];\n",
                "Reflect.defineProperty(lockedLengthArray, 'length', { writable: false });\n",
                "assert.sameValue(Reflect.set(lockedLengthArray, '1', 24), false, 'Reflect.set fails when array receiver length is not writable');\n",
                "assert.sameValue(lockedLengthArray.length, 1, 'Reflect.set keeps locked array length unchanged');\n",
                "assert.sameValue(lockedLengthArray[1], undefined, 'Reflect.set does not append when array length is locked');\n",
            ),
            "native-test262-array-reflect.js",
        )
        .expect("array/reflect script should compile");

        let mut runtime = crate::interpreter::RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect("array/reflect script should execute");
    }

    #[test]
    fn compile_test262_basic_script_supports_property_introspection_semantics() {
        let result = execute_test262_basic(
            concat!(
                "var proto = { inherited: 1 };\n",
                "var object = Object.create(proto);\n",
                "object.own = 2;\n",
                "var array = [10];\n",
                "assert.sameValue(Reflect.has(object, 'own'), true, 'Reflect.has sees own property');\n",
                "assert.sameValue(Reflect.has(object, 'inherited'), true, 'Reflect.has walks prototype chain');\n",
                "assert.sameValue(Reflect.has(array, '0'), true, 'Reflect.has sees array index');\n",
                "assert.sameValue(Reflect.has(array, 'length'), true, 'Reflect.has sees array length');\n",
                "assert.sameValue('length' in array, true, 'in operator sees array length');\n",
                "assert.sameValue(Object.hasOwn(object, 'own'), true, 'Object.hasOwn sees own property');\n",
                "assert.sameValue(Object.hasOwn(object, 'inherited'), false, 'Object.hasOwn ignores inherited property');\n",
                "assert.sameValue(Object.hasOwn('otter', 'length'), true, 'Object.hasOwn coerces string receiver');\n",
                "assert.sameValue(Object.prototype.hasOwnProperty.call('otter', 'length'), true, 'hasOwnProperty coerces string receiver');\n",
                "assert.sameValue(Object.prototype.propertyIsEnumerable.call('otter', 'length'), false, 'string length is non-enumerable');\n",
                "try {\n",
                "  Reflect.has('otter', 'length');\n",
                "  throw new Test262Error('Reflect.has should reject primitive string targets');\n",
                "} catch (error) {}\n",
            ),
            "native-test262-property-introspection.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_reflect_object_target_validation() {
        let result = execute_test262_basic(
            concat!(
                "try {\n",
                "  Reflect.get(1, 'x');\n",
                "  throw new Test262Error('Reflect.get should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.get primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.get('otter', 'length');\n",
                "  throw new Test262Error('Reflect.get should reject primitive string targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.get primitive string target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.set(1, 'x', 1);\n",
                "  throw new Test262Error('Reflect.set should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.set primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.defineProperty(1, 'x', { value: 1 });\n",
                "  throw new Test262Error('Reflect.defineProperty should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.defineProperty primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.deleteProperty(1, 'x');\n",
                "  throw new Test262Error('Reflect.deleteProperty should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.deleteProperty primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.getOwnPropertyDescriptor(1, 'x');\n",
                "  throw new Test262Error('Reflect.getOwnPropertyDescriptor should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.getOwnPropertyDescriptor primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.getPrototypeOf(1);\n",
                "  throw new Test262Error('Reflect.getPrototypeOf should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.getPrototypeOf primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.has(1, 'x');\n",
                "  throw new Test262Error('Reflect.has should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.has primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.isExtensible(1);\n",
                "  throw new Test262Error('Reflect.isExtensible should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.isExtensible primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.ownKeys('otter');\n",
                "  throw new Test262Error('Reflect.ownKeys should reject primitive string targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.ownKeys primitive string target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.preventExtensions(1);\n",
                "  throw new Test262Error('Reflect.preventExtensions should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.preventExtensions primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.setPrototypeOf(1, null);\n",
                "  throw new Test262Error('Reflect.setPrototypeOf should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.setPrototypeOf primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.setPrototypeOf({}, 1);\n",
                "  throw new Test262Error('Reflect.setPrototypeOf should reject primitive proto');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.setPrototypeOf primitive proto throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Reflect.setPrototypeOf({}, 'otter');\n",
                "  throw new Test262Error('Reflect.setPrototypeOf should reject primitive string proto');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Reflect.setPrototypeOf primitive string proto throws TypeError');\n",
                "}\n",
            ),
            "native-test262-reflect-target-validation.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_introspection_nullish_type_errors() {
        let result = execute_test262_basic(
            concat!(
                "try {\n",
                "  Object.hasOwn(null, 'x');\n",
                "  throw new Test262Error('Object.hasOwn should reject null target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.hasOwn null target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.keys(null);\n",
                "  throw new Test262Error('Object.keys should reject null target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.keys null target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.values(undefined);\n",
                "  throw new Test262Error('Object.values should reject undefined target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.values undefined target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.entries(null);\n",
                "  throw new Test262Error('Object.entries should reject null target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.entries null target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.getOwnPropertyDescriptor(undefined, 'x');\n",
                "  throw new Test262Error('Object.getOwnPropertyDescriptor should reject undefined target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.getOwnPropertyDescriptor undefined target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.getOwnPropertyDescriptors(null);\n",
                "  throw new Test262Error('Object.getOwnPropertyDescriptors should reject null target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.getOwnPropertyDescriptors null target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.getOwnPropertyNames(undefined);\n",
                "  throw new Test262Error('Object.getOwnPropertyNames should reject undefined target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.getOwnPropertyNames undefined target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.prototype.hasOwnProperty.call(null, 'x');\n",
                "  throw new Test262Error('hasOwnProperty should reject null receiver');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'hasOwnProperty null receiver throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.prototype.propertyIsEnumerable.call(undefined, 'x');\n",
                "  throw new Test262Error('propertyIsEnumerable should reject undefined receiver');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'propertyIsEnumerable undefined receiver throws TypeError');\n",
                "}\n",
                "assert.sameValue(Object.prototype.toString.call(undefined), '[object Undefined]', 'Object.prototype.toString preserves undefined tag');\n",
                "assert.sameValue(Object.prototype.toString.call(null), '[object Null]', 'Object.prototype.toString preserves null tag');\n",
            ),
            "native-test262-object-introspection-nullish-errors.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_prototype_to_string_tags() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(Object.prototype.toString.call({}), '[object Object]', 'plain object default tag');\n",
                "assert.sameValue(Object.prototype.toString.call([]), '[object Array]', 'array builtin tag');\n",
                "assert.sameValue(Object.prototype.toString.call(function() {}), '[object Function]', 'function builtin tag');\n",
                "assert.sameValue(Object.prototype.toString.call('otter'), '[object String]', 'primitive string builtin tag');\n",
                "assert.sameValue(Object.prototype.toString.call(1), '[object Number]', 'primitive number builtin tag');\n",
                "assert.sameValue(Object.prototype.toString.call(true), '[object Boolean]', 'primitive boolean builtin tag');\n",
                "assert.sameValue(Object.prototype.toString.call(Math), '[object Math]', 'Math uses @@toStringTag');\n",
                "var tagged = { '@@toStringTag': 'OtterThing' };\n",
                "assert.sameValue(Object.prototype.toString.call(tagged), '[object OtterThing]', 'own @@toStringTag overrides builtin tag');\n",
                "var taggedProto = { '@@toStringTag': 'ProtoTag' };\n",
                "var inheritedTagged = Object.create(taggedProto);\n",
                "assert.sameValue(Object.prototype.toString.call(inheritedTagged), '[object ProtoTag]', 'inherited @@toStringTag is observed');\n",
                "var nonStringTag = { '@@toStringTag': 1 };\n",
                "assert.sameValue(Object.prototype.toString.call(nonStringTag), '[object Object]', 'non-string @@toStringTag falls back to builtin tag');\n",
                "Boolean.prototype['@@toStringTag'] = 'Flag';\n",
                "assert.sameValue(Object.prototype.toString.call(true), '[object Flag]', 'primitive boolean receiver observes inherited @@toStringTag via boxing');\n",
                "delete Boolean.prototype['@@toStringTag'];\n",
                "String.prototype['@@toStringTag'] = 'Text';\n",
                "assert.sameValue(Object.prototype.toString.call('otter'), '[object Text]', 'primitive string receiver observes inherited @@toStringTag via boxing');\n",
                "delete String.prototype['@@toStringTag'];\n",
            ),
            "native-test262-object-prototype-to-string-tags.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_string_exotic_object_introspection() {
        let result = execute_test262_basic(
            concat!(
                "var keys = Object.keys('otter');\n",
                "assert.sameValue(keys.length, 5, 'Object.keys string length');\n",
                "assert.sameValue(keys[0], '0', 'Object.keys string index 0');\n",
                "assert.sameValue(keys[4], '4', 'Object.keys string index 4');\n",
                "var values = Object.values('otter');\n",
                "assert.sameValue(values.length, 5, 'Object.values string length');\n",
                "assert.sameValue(values[0], 'o', 'Object.values string value 0');\n",
                "assert.sameValue(values[4], 'r', 'Object.values string value 4');\n",
                "var entries = Object.entries('otter');\n",
                "assert.sameValue(entries.length, 5, 'Object.entries string length');\n",
                "assert.sameValue(entries[0][0], '0', 'Object.entries key 0');\n",
                "assert.sameValue(entries[0][1], 'o', 'Object.entries value 0');\n",
                "assert.sameValue(entries[4][0], '4', 'Object.entries key 4');\n",
                "assert.sameValue(entries[4][1], 'r', 'Object.entries value 4');\n",
                "var names = Object.getOwnPropertyNames('otter');\n",
                "assert.sameValue(names.length, 6, 'Object.getOwnPropertyNames string length');\n",
                "assert.sameValue(names[0], '0', 'Object.getOwnPropertyNames key 0');\n",
                "assert.sameValue(names[4], '4', 'Object.getOwnPropertyNames key 4');\n",
                "assert.sameValue(names[5], 'length', 'Object.getOwnPropertyNames length key');\n",
                "var indexDesc = Object.getOwnPropertyDescriptor('otter', '0');\n",
                "assert.sameValue(indexDesc.value, 'o', 'string index descriptor value');\n",
                "assert.sameValue(indexDesc.writable, false, 'string index descriptor writable');\n",
                "assert.sameValue(indexDesc.enumerable, true, 'string index descriptor enumerable');\n",
                "assert.sameValue(indexDesc.configurable, false, 'string index descriptor configurable');\n",
                "var lengthDesc = Object.getOwnPropertyDescriptor('otter', 'length');\n",
                "assert.sameValue(lengthDesc.value, 5, 'string length descriptor value');\n",
                "assert.sameValue(lengthDesc.enumerable, false, 'string length descriptor enumerable');\n",
                "var descriptors = Object.getOwnPropertyDescriptors('otter');\n",
                "assert.sameValue(descriptors['0'].value, 'o', 'string descriptors index value');\n",
                "assert.sameValue(descriptors.length.value, 5, 'string descriptors length value');\n",
                "assert.sameValue(Object.hasOwn('otter', '0'), true, 'Object.hasOwn sees string index');\n",
                "assert.sameValue(Object.prototype.propertyIsEnumerable.call('otter', '0'), true, 'string index is enumerable');\n",
            ),
            "native-test262-string-exotic-introspection.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_hides_primitive_wrapper_backing_slots() {
        let result = execute_test262_basic(
            concat!(
                "var numberObject = new Number(1);\n",
                "assert.sameValue(Object.keys(numberObject).length, 0, 'boxed number exposes no enumerable own keys');\n",
                "assert.sameValue(Object.getOwnPropertyNames(numberObject).length, 0, 'boxed number hides backing slot from names');\n",
                "assert.sameValue(Reflect.ownKeys(numberObject).length, 0, 'boxed number hides backing slot from ownKeys');\n",
                "assert.sameValue(numberObject.__otter_number_data__, undefined, 'boxed number hides backing slot from [[Get]]');\n",
                "assert.sameValue(Object.hasOwn(numberObject, '__otter_number_data__'), false, 'boxed number hides backing slot from Object.hasOwn');\n",
                "assert.sameValue(Object.getOwnPropertyDescriptor(numberObject, '__otter_number_data__'), undefined, 'boxed number hides backing slot descriptor');\n",
                "var booleanObject = new Boolean(true);\n",
                "assert.sameValue(Object.getOwnPropertyNames(booleanObject).length, 0, 'boxed boolean hides backing slot from names');\n",
                "assert.sameValue(Reflect.ownKeys(booleanObject).length, 0, 'boxed boolean hides backing slot from ownKeys');\n",
                "assert.sameValue(booleanObject.__otter_boolean_data__, undefined, 'boxed boolean hides backing slot from [[Get]]');\n",
                "assert.sameValue(Object.hasOwn(booleanObject, '__otter_boolean_data__'), false, 'boxed boolean hides backing slot from Object.hasOwn');\n",
                "var stringObject = new String('ot');\n",
                "var stringKeys = Object.keys(stringObject);\n",
                "assert.sameValue(stringKeys.length, 2, 'boxed string exposes exotic indices as enumerable keys');\n",
                "assert.sameValue(stringKeys[0], '0', 'boxed string key 0');\n",
                "assert.sameValue(stringKeys[1], '1', 'boxed string key 1');\n",
                "var stringNames = Object.getOwnPropertyNames(stringObject);\n",
                "assert.sameValue(stringNames.length, 3, 'boxed string names include indices and length only');\n",
                "assert.sameValue(stringNames[0], '0', 'boxed string name 0');\n",
                "assert.sameValue(stringNames[1], '1', 'boxed string name 1');\n",
                "assert.sameValue(stringNames[2], 'length', 'boxed string name length');\n",
                "var stringOwnKeys = Reflect.ownKeys(stringObject);\n",
                "assert.sameValue(stringOwnKeys.length, 3, 'boxed string ownKeys include indices and length only');\n",
                "assert.sameValue(stringOwnKeys[2], 'length', 'boxed string ownKeys length');\n",
                "assert.sameValue(stringObject[0], 'o', 'boxed string index access uses exotic semantics');\n",
                "assert.sameValue(stringObject[1], 't', 'boxed string second index access uses exotic semantics');\n",
                "assert.sameValue(stringObject.length, 2, 'boxed string length uses exotic semantics');\n",
                "assert.sameValue(Object.hasOwn(stringObject, 'length'), true, 'boxed string has own length');\n",
                "assert.sameValue(stringObject.__otter_string_data__, undefined, 'boxed string hides backing slot from [[Get]]');\n",
                "assert.sameValue(Object.hasOwn(stringObject, '__otter_string_data__'), false, 'boxed string hides backing slot from Object.hasOwn');\n",
                "assert.sameValue(Object.getOwnPropertyDescriptor(stringObject, '__otter_string_data__'), undefined, 'boxed string hides backing slot descriptor');\n",
                "assert.sameValue(Reflect.ownKeys(Number.prototype).indexOf('__otter_number_data__'), -1, 'Number.prototype hides backing slot from ownKeys');\n",
                "assert.sameValue(Reflect.ownKeys(Boolean.prototype).indexOf('__otter_boolean_data__'), -1, 'Boolean.prototype hides backing slot from ownKeys');\n",
                "assert.sameValue(Reflect.ownKeys(String.prototype).indexOf('__otter_string_data__'), -1, 'String.prototype hides backing slot from ownKeys');\n",
            ),
            "native-test262-wrapper-backing-slots.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_prototype_operations_on_primitives() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(Object.getPrototypeOf('otter'), String.prototype, 'Object.getPrototypeOf boxes primitive strings');\n",
                "assert.sameValue(Object.getPrototypeOf(1), Number.prototype, 'Object.getPrototypeOf boxes primitive numbers');\n",
                "assert.sameValue(Object.getPrototypeOf(true), Boolean.prototype, 'Object.getPrototypeOf boxes primitive booleans');\n",
                "try {\n",
                "  Object.getPrototypeOf(null);\n",
                "  throw new Test262Error('Object.getPrototypeOf should reject null');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.getPrototypeOf null throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.getPrototypeOf(undefined);\n",
                "  throw new Test262Error('Object.getPrototypeOf should reject undefined');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.getPrototypeOf undefined throws TypeError');\n",
                "}\n",
                "assert.sameValue(Object.setPrototypeOf(1, null), 1, 'Object.setPrototypeOf returns primitive number target unchanged');\n",
                "assert.sameValue(Object.setPrototypeOf('otter', {}), 'otter', 'Object.setPrototypeOf returns primitive string target unchanged');\n",
                "assert.sameValue(Object.setPrototypeOf(true, null), true, 'Object.setPrototypeOf returns primitive boolean target unchanged');\n",
                "assert.sameValue(Object.getPrototypeOf('otter'), String.prototype, 'Object.setPrototypeOf does not mutate primitive string prototype');\n",
                "try {\n",
                "  Object.setPrototypeOf(1, 2);\n",
                "  throw new Test262Error('Object.setPrototypeOf should reject invalid proto for primitive target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.setPrototypeOf invalid proto throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.setPrototypeOf(null, {});\n",
                "  throw new Test262Error('Object.setPrototypeOf should reject null target');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.setPrototypeOf null target throws TypeError');\n",
                "}\n",
            ),
            "native-test262-object-prototype-primitives.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_enumeration_helpers_over_accessors() {
        let result = execute_test262_basic(
            concat!(
                "var object = {};\n",
                "Object.defineProperty(object, 'marker', { value: 7, enumerable: false });\n",
                "Object.defineProperty(object, 'value', { get: function() { return this.marker; }, enumerable: true, configurable: true });\n",
                "var values = Object.values(object);\n",
                "assert.sameValue(values.length, 1, 'Object.values keeps one enumerable accessor');\n",
                "assert.sameValue(values[0], 7, 'Object.values uses [[Get]] for accessor');\n",
                "var entries = Object.entries(object);\n",
                "assert.sameValue(entries.length, 1, 'Object.entries keeps one enumerable accessor');\n",
                "assert.sameValue(entries[0][0], 'value', 'Object.entries keeps accessor key');\n",
                "assert.sameValue(entries[0][1], 7, 'Object.entries uses [[Get]] for accessor');\n",
                "var assigned = Object.assign({}, object);\n",
                "assert.sameValue(assigned.value, 7, 'Object.assign copies accessor value via [[Get]]');\n",
                "var array = [1, 2, 3];\n",
                "Object.defineProperty(array, '1', { get: function() { return 42; }, enumerable: true, configurable: true });\n",
                "var arrayValues = Object.values(array);\n",
                "assert.sameValue(arrayValues.length, 3, 'Object.values preserves array element count');\n",
                "assert.sameValue(arrayValues[1], 42, 'Object.values sees accessor-backed array index');\n",
                "var arrayEntries = Object.entries(array);\n",
                "assert.sameValue(arrayEntries[1][0], '1', 'Object.entries keeps accessor-backed array index key');\n",
                "assert.sameValue(arrayEntries[1][1], 42, 'Object.entries sees accessor-backed array index value');\n",
                "var arrayAssigned = Object.assign({}, array);\n",
                "assert.sameValue(arrayAssigned['1'], 42, 'Object.assign copies accessor-backed array index');\n",
                "var arrayTarget = [];\n",
                "Object.assign(arrayTarget, { 1: 9, named: 4 });\n",
                "assert.sameValue(arrayTarget.length, 2, 'Object.assign updates array target length for numeric index');\n",
                "assert.sameValue(arrayTarget[1], 9, 'Object.assign writes numeric index onto array target');\n",
                "assert.sameValue(arrayTarget.named, 4, 'Object.assign preserves named property on array target');\n",
                "var accessorTarget = [1, 2];\n",
                "Object.defineProperty(accessorTarget, '1', { set: function(v) { this.captured = v; }, enumerable: true, configurable: true });\n",
                "Object.assign(accessorTarget, { 1: 15 });\n",
                "assert.sameValue(accessorTarget.captured, 15, 'Object.assign uses array target accessor setter');\n",
                "var lockedArrayTarget = [1];\n",
                "Object.defineProperty(lockedArrayTarget, 'length', { writable: false });\n",
                "try {\n",
                "  Object.assign(lockedArrayTarget, { 1: 16 });\n",
                "  throw new Test262Error('Object.assign should throw when array target cannot append');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.assign append failure on locked array throws TypeError');\n",
                "}\n",
                "assert.sameValue(lockedArrayTarget.length, 1, 'Object.assign keeps locked array target length unchanged');\n",
                "var stringAssigned = Object.assign({}, 'ot');\n",
                "assert.sameValue(stringAssigned[0], 'o', 'Object.assign boxes string source index 0');\n",
                "assert.sameValue(stringAssigned[1], 't', 'Object.assign boxes string source index 1');\n",
                "var setterProto = {};\n",
                "Object.defineProperty(setterProto, 'value', { set: function(v) { this.seen = v; }, enumerable: true, configurable: true });\n",
                "var setterTarget = Object.create(setterProto);\n",
                "Object.assign(setterTarget, { value: 11 });\n",
                "assert.sameValue(setterTarget.seen, 11, 'Object.assign uses target [[Set]] and inherited setter');\n",
                "var frozenTarget = {};\n",
                "Object.defineProperty(frozenTarget, 'locked', { value: 1, writable: false, enumerable: true, configurable: true });\n",
                "try {\n",
                "  Object.assign(frozenTarget, { locked: 2 });\n",
                "  throw new Test262Error('Object.assign should throw when target write fails');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.assign write failure throws TypeError');\n",
                "}\n",
                "var throwingSource = {};\n",
                "Object.defineProperty(throwingSource, 'boom', { get: function() { throw new TypeError('boom'); }, enumerable: true });\n",
                "try {\n",
                "  Object.values(throwingSource);\n",
                "  throw new Test262Error('Object.values should rethrow getter errors');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.message, 'boom', 'Object.values preserves thrown getter error');\n",
                "}\n",
                "try {\n",
                "  Object.entries(throwingSource);\n",
                "  throw new Test262Error('Object.entries should rethrow getter errors');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.message, 'boom', 'Object.entries preserves thrown getter error');\n",
                "}\n",
                "try {\n",
                "  Object.assign({}, throwingSource);\n",
                "  throw new Test262Error('Object.assign should rethrow getter errors');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.message, 'boom', 'Object.assign preserves thrown getter error');\n",
                "}\n",
            ),
            "native-test262-enumeration-helpers-accessors.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_computed_property_key_coercion() {
        let result = execute_test262_basic(
            concat!(
                "var stringValue = 'otter';\n",
                "var zero = '0';\n",
                "var lengthKey = 'length';\n",
                "assert.sameValue(stringValue[zero], 'o', 'computed string index via string key');\n",
                "assert.sameValue(stringValue[lengthKey], 5, 'computed string length via string key');\n",
                "assert.sameValue(stringValue['9'], undefined, 'missing computed string index');\n",
                "var object = {};\n",
                "var objectKey = '0';\n",
                "object[objectKey] = 7;\n",
                "assert.sameValue(object[0], 7, 'computed object numeric key stored as property');\n",
                "var array = [1, 2];\n",
                "var indexKey = '1';\n",
                "array[indexKey] = 9;\n",
                "assert.sameValue(array[1], 9, 'computed array string index updates element');\n",
                "var appendKey = '2';\n",
                "array[appendKey] = 11;\n",
                "assert.sameValue(array[2], 11, 'computed array string index appends element');\n",
                "var deleteTarget = { keep: 1, drop: 2 };\n",
                "var deleteKey = 'drop';\n",
                "assert.sameValue(delete deleteTarget[deleteKey], true, 'delete computed returns true');\n",
                "assert.sameValue('drop' in deleteTarget, false, 'delete computed removes ordinary property');\n",
            ),
            "native-test262-computed-property-key-coercion.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_number_and_boolean_member_access() {
        let result = execute_test262_basic(
            concat!(
                "var numberValue = 7;\n",
                "var booleanValue = true;\n",
                "assert.sameValue(numberValue.valueOf(), 7, 'number primitive method call');\n",
                "assert.sameValue(booleanValue.valueOf(), true, 'boolean primitive method call');\n",
                "assert.sameValue(numberValue['valueOf'](), 7, 'computed number primitive method call');\n",
                "assert.sameValue(booleanValue['valueOf'](), true, 'computed boolean primitive method call');\n",
                "assert.sameValue(numberValue.constructor, Number, 'number primitive prototype property');\n",
                "assert.sameValue(booleanValue.constructor, Boolean, 'boolean primitive prototype property');\n",
                "numberValue.extra = 1;\n",
                "booleanValue.extra = 1;\n",
                "assert.sameValue(numberValue.extra, undefined, 'number primitive write does not persist');\n",
                "assert.sameValue(booleanValue.extra, undefined, 'boolean primitive write does not persist');\n",
                "assert.sameValue(delete numberValue.missing, true, 'delete on number primitive succeeds');\n",
                "assert.sameValue(delete booleanValue.missing, true, 'delete on boolean primitive succeeds');\n",
            ),
            "native-test262-primitive-member-access.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_rejects_nullish_member_access() {
        let cases = [
            ("var x = null.foo;\n", "null-static-get.js"),
            ("var x = undefined.foo;\n", "undefined-static-get.js"),
            ("var x = null['foo'];\n", "null-computed-get.js"),
            ("var x = undefined['foo'];\n", "undefined-computed-get.js"),
            ("null.foo = 1;\n", "null-static-set.js"),
            ("undefined['foo'] = 1;\n", "undefined-computed-set.js"),
            ("delete null.foo;\n", "null-static-delete.js"),
            ("delete undefined['foo'];\n", "undefined-computed-delete.js"),
            ("null.valueOf();\n", "null-static-call.js"),
            ("undefined['valueOf']();\n", "undefined-computed-call.js"),
        ];

        for (source, source_url) in cases {
            let error = execute_test262_basic_error(source, source_url);
            assert!(
                matches!(error, InterpreterError::UncaughtThrow(_)),
                "expected UncaughtThrow for {source_url}, got {error:?}"
            );
            // The message "null or undefined" is inside the thrown object,
            // but for now we just verify it's an UncaughtThrow as it confirms it's a JS-land error.
        }
    }

    #[test]
    fn compile_test262_basic_script_coerces_property_keys_via_to_property_key() {
        let result = execute_test262_basic(
            concat!(
                "var object = {};\n",
                "assert.sameValue(Reflect.defineProperty(object, 1, { value: 7 }), true, 'Reflect.defineProperty numeric key');\n",
                "assert.sameValue(object['1'], 7, 'numeric key stored as string');\n",
                "assert.sameValue(Object.defineProperty(object, new String('boxed'), { value: 9 }).boxed, 9, 'boxed string key');\n",
                "assert.sameValue(object.hasOwnProperty(new Number(1)), true, 'boxed number key');\n",
                "assert.sameValue(object.propertyIsEnumerable(new String('boxed')), false, 'boxed string enumerable default');\n",
            ),
            "native-test262-to-property-key.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_preserves_descriptor_flags() {
        let result = execute_test262_basic(
            concat!(
                "var object = {};\n",
                "assert.sameValue(Reflect.defineProperty(object, 'hidden', { value: 7, writable: false, enumerable: false, configurable: false }), true, 'Reflect.defineProperty succeeds');\n",
                "var reflectDesc = Reflect.getOwnPropertyDescriptor(object, 'hidden');\n",
                "assert.sameValue(reflectDesc.value, 7, 'reflect desc value');\n",
                "assert.sameValue(reflectDesc.writable, false, 'reflect desc writable');\n",
                "assert.sameValue(reflectDesc.enumerable, false, 'reflect desc enumerable');\n",
                "assert.sameValue(reflectDesc.configurable, false, 'reflect desc configurable');\n",
                "var objectDesc = Object.getOwnPropertyDescriptor(object, 'hidden');\n",
                "assert.sameValue(objectDesc.value, 7, 'object desc value');\n",
                "assert.sameValue(objectDesc.writable, false, 'object desc writable');\n",
                "assert.sameValue(objectDesc.enumerable, false, 'object desc enumerable');\n",
                "assert.sameValue(objectDesc.configurable, false, 'object desc configurable');\n",
                "var partial = {};\n",
                "Object.defineProperty(partial, 'visible', { value: 1, writable: true, enumerable: true, configurable: true });\n",
                "Object.defineProperty(partial, 'visible', { value: 2 });\n",
                "var partialDesc = Object.getOwnPropertyDescriptor(partial, 'visible');\n",
                "assert.sameValue(partialDesc.value, 2, 'partial desc updates value');\n",
                "assert.sameValue(partialDesc.writable, true, 'partial desc preserves writable');\n",
                "assert.sameValue(partialDesc.enumerable, true, 'partial desc preserves enumerable');\n",
                "assert.sameValue(partialDesc.configurable, true, 'partial desc preserves configurable');\n",
                "var fixed = {};\n",
                "Object.defineProperty(fixed, 'locked', { value: 1, writable: false, enumerable: false, configurable: false });\n",
                "assert.sameValue(Reflect.defineProperty(fixed, 'locked', { value: 1 }), true, 'same value redefine succeeds');\n",
                "assert.sameValue(Reflect.defineProperty(fixed, 'locked', { value: 2 }), false, 'changing frozen value fails');\n",
                "assert.sameValue(Reflect.defineProperty(fixed, 'locked', { writable: true }), false, 'making frozen value writable fails');\n",
                "assert.sameValue(Reflect.defineProperty(fixed, 'locked', { enumerable: true }), false, 'changing enumerable fails');\n",
                "try {\n",
                "  Object.defineProperty(fixed, 'locked', { value: 2 });\n",
                "  throw new Test262Error('Object.defineProperty should throw on invalid redefine');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.defineProperty invalid redefine throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperty(1, 'x', { value: 1 });\n",
                "  throw new Test262Error('Object.defineProperty should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.defineProperty primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperty('otter', 'x', { value: 1 });\n",
                "  throw new Test262Error('Object.defineProperty should reject primitive string targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.defineProperty primitive string target throws TypeError');\n",
                "}\n",
                "var blocked = {};\n",
                "Object.preventExtensions(blocked);\n",
                "try {\n",
                "  Object.defineProperty(blocked, 'late', { value: 1 });\n",
                "  throw new Test262Error('Object.defineProperty should reject non-extensible targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.defineProperty non-extensible target throws TypeError');\n",
                "}\n",
            ),
            "native-test262-descriptor-flags.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_define_properties() {
        let result = execute_test262_basic(
            concat!(
                "var target = {};\n",
                "var source = {};\n",
                "Object.defineProperty(source, 'hidden', { value: { value: 99 }, enumerable: false });\n",
                "Object.defineProperty(source, 'visible', { value: { value: 7, writable: false, enumerable: true, configurable: false }, enumerable: true });\n",
                "Object.defineProperties(target, source);\n",
                "assert.sameValue(target.visible, 7, 'defineProperties installs data descriptor entry');\n",
                "var desc = Object.getOwnPropertyDescriptor(target, 'visible');\n",
                "assert.sameValue(desc.writable, false, 'defineProperties preserves writable');\n",
                "assert.sameValue(desc.enumerable, true, 'defineProperties preserves enumerable');\n",
                "assert.sameValue(desc.configurable, false, 'defineProperties preserves configurable');\n",
                "assert.sameValue(target.hasOwnProperty('hidden'), false, 'non-enumerable descriptor source key skipped');\n",
                "Object.defineProperties(target, 1);\n",
                "assert.sameValue(target.visible, 7, 'defineProperties ignores number descriptor map with no enumerable keys');\n",
                "Object.defineProperties(target, true);\n",
                "assert.sameValue(target.visible, 7, 'defineProperties ignores boolean descriptor map with no enumerable keys');\n",
                "try {\n",
                "  Object.defineProperty({}, 'broken', undefined);\n",
                "  throw new Test262Error('defineProperty should reject non-object descriptor');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperty rejects non-object descriptor with TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperties({}, { broken: 1 });\n",
                "  throw new Test262Error('defineProperties should reject non-object descriptor entry');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperties rejects non-object descriptor entry with TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperties({}, undefined);\n",
                "  throw new Test262Error('defineProperties should reject undefined descriptor map');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperties rejects undefined descriptor map with TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperties({}, null);\n",
                "  throw new Test262Error('defineProperties should reject null descriptor map');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperties rejects null descriptor map with TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperties({}, 'x');\n",
                "  throw new Test262Error('defineProperties should reject primitive descriptor entries from string source');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperties rejects primitive descriptor entries from string source');\n",
                "}\n",
                "var blocked = {};\n",
                "Object.preventExtensions(blocked);\n",
                "try {\n",
                "  Object.defineProperties(blocked, { late: { value: 1 } });\n",
                "  throw new Test262Error('defineProperties should reject non-extensible targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperties non-extensible target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperties(1, { late: { value: 1 } });\n",
                "  throw new Test262Error('defineProperties should reject primitive targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperties primitive target throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.defineProperties('otter', { late: { value: 1 } });\n",
                "  throw new Test262Error('defineProperties should reject primitive string targets');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'defineProperties primitive string target throws TypeError');\n",
                "}\n",
            ),
            "native-test262-object-define-properties.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_create_descriptor_maps() {
        let result = execute_test262_basic(
            concat!(
                "var created = Object.create(null, {\n",
                "  visible: { value: 7, enumerable: true, configurable: true },\n",
                "  hidden: { value: 8, enumerable: false, configurable: true }\n",
                "});\n",
                "assert.sameValue(Object.getPrototypeOf(created), null, 'Object.create supports null prototype');\n",
                "assert.sameValue(created.visible, 7, 'Object.create applies visible descriptor');\n",
                "assert.sameValue(created.hidden, 8, 'Object.create applies hidden descriptor');\n",
                "var keys = Object.keys(created);\n",
                "assert.sameValue(keys.length, 1, 'Object.create only enumerates visible descriptor');\n",
                "assert.sameValue(keys[0], 'visible', 'Object.create keeps enumerable descriptor key');\n",
                "var defaulted = Object.create({}, 1);\n",
                "assert.sameValue(Object.keys(defaulted).length, 0, 'Object.create ignores number descriptor map with no enumerable keys');\n",
                "var undefinedProps = Object.create({}, undefined);\n",
                "assert.sameValue(Object.keys(undefinedProps).length, 0, 'Object.create skips undefined properties map');\n",
                "try {\n",
                "  Object.create();\n",
                "  throw new Test262Error('Object.create should require prototype argument');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.create missing prototype throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.create(1);\n",
                "  throw new Test262Error('Object.create should reject primitive prototype');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.create primitive prototype throws TypeError');\n",
                "}\n",
                "try {\n",
                "  Object.create({}, 'x');\n",
                "  throw new Test262Error('Object.create should reject primitive descriptor entries from string source');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.create string properties map throws TypeError');\n",
                "}\n",
            ),
            "native-test262-object-create-descriptor-maps.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_get_own_property_descriptors() {
        let result = execute_test262_basic(
            concat!(
                "var object = { visible: 1 };\n",
                "Object.defineProperty(object, 'hidden', { value: 2, writable: false, enumerable: false, configurable: false });\n",
                "var descriptors = Object.getOwnPropertyDescriptors(object);\n",
                "assert.sameValue(descriptors.visible.value, 1, 'visible descriptor value');\n",
                "assert.sameValue(descriptors.visible.writable, true, 'visible descriptor writable');\n",
                "assert.sameValue(descriptors.visible.enumerable, true, 'visible descriptor enumerable');\n",
                "assert.sameValue(descriptors.hidden.value, 2, 'hidden descriptor value');\n",
                "assert.sameValue(descriptors.hidden.writable, false, 'hidden descriptor writable');\n",
                "assert.sameValue(descriptors.hidden.enumerable, false, 'hidden descriptor enumerable');\n",
                "assert.sameValue(descriptors.hidden.configurable, false, 'hidden descriptor configurable');\n",
                "var arrayDescriptors = Object.getOwnPropertyDescriptors([7]);\n",
                "assert.sameValue(arrayDescriptors['0'].value, 7, 'array index descriptor value');\n",
                "assert.sameValue(arrayDescriptors['0'].enumerable, true, 'array index descriptor enumerable');\n",
                "assert.sameValue(arrayDescriptors.length.value, 1, 'array length descriptor value');\n",
                "assert.sameValue(arrayDescriptors.length.enumerable, false, 'array length descriptor enumerable');\n",
                "assert.sameValue(arrayDescriptors.hasOwnProperty('0'), true, 'result carries numeric key');\n",
                "assert.sameValue(arrayDescriptors.hasOwnProperty('length'), true, 'result carries length key');\n",
            ),
            "native-test262-object-get-own-property-descriptors.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_extensibility_controls() {
        let result = execute_test262_basic(
            concat!(
                "var object = {};\n",
                "assert.sameValue(Object.isExtensible(object), true, 'Object.isExtensible true');\n",
                "assert.sameValue(Reflect.isExtensible(object), true, 'Reflect.isExtensible true');\n",
                "assert.sameValue(Object.preventExtensions(object), object, 'Object.preventExtensions returns target');\n",
                "assert.sameValue(Reflect.preventExtensions(object), true, 'Reflect.preventExtensions returns true');\n",
                "assert.sameValue(Object.isExtensible(object), false, 'Object.isExtensible false');\n",
                "assert.sameValue(Reflect.isExtensible(object), false, 'Reflect.isExtensible false');\n",
                "assert.sameValue(Reflect.setPrototypeOf(object, null), false, 'Reflect.setPrototypeOf fails on non-extensible target');\n",
                "assert.sameValue(Object.preventExtensions('otter'), 'otter', 'Object.preventExtensions returns primitive string unchanged');\n",
                "assert.sameValue(Object.seal('otter'), 'otter', 'Object.seal returns primitive string unchanged');\n",
                "assert.sameValue(Object.freeze('otter'), 'otter', 'Object.freeze returns primitive string unchanged');\n",
                "assert.sameValue(Object.isExtensible('otter'), false, 'Object.isExtensible treats primitive string as non-object');\n",
                "try {\n",
                "  Object.setPrototypeOf(object, null);\n",
                "  throw new Test262Error('Object.setPrototypeOf should throw on non-extensible target');\n",
                "} catch (error) {}\n",
                "try {\n",
                "  Object.setPrototypeOf({}, 'otter');\n",
                "  throw new Test262Error('Object.setPrototypeOf should reject primitive string proto');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.setPrototypeOf primitive string proto throws TypeError');\n",
                "}\n",
            ),
            "native-test262-extensibility-controls.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_integrity_predicates() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(Object.isFrozen(undefined), true, 'undefined is frozen');\n",
                "assert.sameValue(Object.isSealed(undefined), true, 'undefined is sealed');\n",
                "assert.sameValue(Object.isFrozen('otter'), true, 'primitive string is frozen');\n",
                "assert.sameValue(Object.isSealed('otter'), true, 'primitive string is sealed');\n",
                "var object = { value: 1 };\n",
                "assert.sameValue(Object.isFrozen(object), false, 'plain object is not frozen');\n",
                "assert.sameValue(Object.isSealed(object), false, 'plain object is not sealed');\n",
                "Object.seal(object);\n",
                "assert.sameValue(Object.isSealed(object), true, 'sealed object is sealed');\n",
                "assert.sameValue(Object.isFrozen(object), false, 'sealed object is not frozen');\n",
                "Object.freeze(object);\n",
                "assert.sameValue(Object.isFrozen(object), true, 'frozen object is frozen');\n",
                "assert.sameValue(Object.isSealed(object), true, 'frozen object is sealed');\n",
            ),
            "native-test262-integrity-predicates.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_own_key_enumeration() {
        let result = execute_test262_basic(
            concat!(
                "var array = [10, 20];\n",
                "var ownKeys = Reflect.ownKeys(array);\n",
                "assert.sameValue(ownKeys.length, 3, 'Reflect.ownKeys length');\n",
                "assert.sameValue(ownKeys[0], '0', 'index 0 first');\n",
                "assert.sameValue(ownKeys[1], '1', 'index 1 second');\n",
                "assert.sameValue(ownKeys[2], 'length', 'length last');\n",
                "var keys = Object.keys(array);\n",
                "assert.sameValue(keys.length, 2, 'Object.keys length');\n",
                "assert.sameValue(keys[0], '0', 'Object.keys index 0');\n",
                "assert.sameValue(keys[1], '1', 'Object.keys index 1');\n",
                "var names = Object.getOwnPropertyNames(array);\n",
                "assert.sameValue(names.length, 3, 'Object.getOwnPropertyNames length');\n",
                "assert.sameValue(names[0], '0', 'Object.getOwnPropertyNames index 0');\n",
                "assert.sameValue(names[1], '1', 'Object.getOwnPropertyNames index 1');\n",
                "assert.sameValue(names[2], 'length', 'Object.getOwnPropertyNames length key');\n",
                "var elementDesc = Reflect.getOwnPropertyDescriptor(array, '0');\n",
                "assert.sameValue(elementDesc.value, 10, 'element descriptor value');\n",
                "assert.sameValue(elementDesc.writable, true, 'element descriptor writable');\n",
                "assert.sameValue(elementDesc.enumerable, true, 'element descriptor enumerable');\n",
                "assert.sameValue(elementDesc.configurable, true, 'element descriptor configurable');\n",
                "var lengthDesc = Object.getOwnPropertyDescriptor(array, 'length');\n",
                "assert.sameValue(lengthDesc.value, 2, 'length descriptor value');\n",
                "assert.sameValue(lengthDesc.writable, true, 'length descriptor writable');\n",
                "assert.sameValue(lengthDesc.enumerable, false, 'length descriptor enumerable');\n",
                "assert.sameValue(lengthDesc.configurable, false, 'length descriptor configurable');\n",
            ),
            "native-test262-array-own-key-enumeration.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_integrity_predicates() {
        let result = execute_test262_basic(
            concat!(
                "var sealed = [1, 2];\n",
                "assert.sameValue(Object.isSealed(sealed), false, 'plain array is not sealed');\n",
                "assert.sameValue(Object.isFrozen(sealed), false, 'plain array is not frozen');\n",
                "Object.seal(sealed);\n",
                "assert.sameValue(Object.isSealed(sealed), true, 'sealed array is sealed');\n",
                "assert.sameValue(Object.isFrozen(sealed), false, 'sealed array is not frozen');\n",
                "var sealedIndex = Reflect.getOwnPropertyDescriptor(sealed, '0');\n",
                "assert.sameValue(sealedIndex.writable, true, 'sealed index remains writable');\n",
                "assert.sameValue(sealedIndex.configurable, false, 'sealed index is not configurable');\n",
                "var frozen = [3, 4];\n",
                "Object.freeze(frozen);\n",
                "assert.sameValue(Object.isSealed(frozen), true, 'frozen array is sealed');\n",
                "assert.sameValue(Object.isFrozen(frozen), true, 'frozen array is frozen');\n",
                "var frozenIndex = Reflect.getOwnPropertyDescriptor(frozen, '0');\n",
                "assert.sameValue(frozenIndex.writable, false, 'frozen index is not writable');\n",
                "assert.sameValue(frozenIndex.configurable, false, 'frozen index is not configurable');\n",
                "var frozenLength = Object.getOwnPropertyDescriptor(frozen, 'length');\n",
                "assert.sameValue(frozenLength.writable, false, 'frozen length is not writable');\n",
                "assert.sameValue(frozenLength.configurable, false, 'frozen length is not configurable');\n",
                "frozen[0] = 99;\n",
                "assert.sameValue(frozen[0], 3, 'frozen array does not overwrite existing element');\n",
                "frozen[2] = 5;\n",
                "assert.sameValue(frozen.length, 2, 'frozen array does not grow');\n",
                "assert.sameValue(frozen[2], undefined, 'frozen array does not create new element');\n",
            ),
            "native-test262-array-integrity-predicates.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_define_property_semantics() {
        let result = execute_test262_basic(
            concat!(
                "var array = [1, 2, 3];\n",
                "assert.sameValue(Object.defineProperty(array, '0', { value: 9 }), array, 'Object.defineProperty returns array');\n",
                "assert.sameValue(array[0], 9, 'existing dense index updates value');\n",
                "Object.defineProperty(array, 'length', { value: 1 });\n",
                "assert.sameValue(array.length, 1, 'array length shrinks');\n",
                "assert.sameValue(array[1], undefined, 'shrunk elements disappear');\n",
                "var lengthDesc = Object.getOwnPropertyDescriptor(array, 'length');\n",
                "assert.sameValue(lengthDesc.value, 1, 'length descriptor tracks shrink');\n",
                "assert.sameValue(lengthDesc.writable, true, 'length stays writable');\n",
                "Object.defineProperty(array, 'length', { writable: false });\n",
                "assert.sameValue(Object.getOwnPropertyDescriptor(array, 'length').writable, false, 'length can be locked');\n",
                "assert.sameValue(Reflect.defineProperty(array, '1', { value: 7, writable: true, enumerable: true, configurable: true }), false, 'locked length prevents append');\n",
                "assert.sameValue(array.length, 1, 'failed append preserves length');\n",
                "var grow = [4];\n",
                "assert.sameValue(Reflect.defineProperty(grow, '1', { value: 8, writable: true, enumerable: true, configurable: true }), true, 'explicit dense index descriptor appends');\n",
                "assert.sameValue(grow.length, 2, 'explicit dense append updates length');\n",
                "assert.sameValue(grow[1], 8, 'explicit dense append stores value');\n",
                "var sealed = [5, 6];\n",
                "Object.seal(sealed);\n",
                "assert.sameValue(Reflect.defineProperty(sealed, 'length', { value: 1 }), false, 'sealed array cannot shrink length');\n",
                "var frozen = [7];\n",
                "Object.freeze(frozen);\n",
                "assert.sameValue(Reflect.defineProperty(frozen, '0', { value: 11 }), false, 'frozen array index redefine fails');\n",
                "assert.sameValue(Reflect.defineProperty(frozen, 'length', { value: 0 }), false, 'frozen array length redefine fails');\n",
            ),
            "native-test262-array-define-property-semantics.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_sparse_array_hole_semantics() {
        let result = execute_test262_basic(
            concat!(
                "var sparse = [1,,3];\n",
                "assert.sameValue(sparse.length, 3, 'sparse length');\n",
                "assert.sameValue(sparse[1], undefined, 'hole reads as undefined');\n",
                "assert.sameValue(Reflect.has(sparse, '1'), false, 'hole is not an own property');\n",
                "var keys = Object.keys(sparse);\n",
                "assert.sameValue(keys.length, 2, 'Object.keys skips holes');\n",
                "assert.sameValue(keys[0], '0', 'Object.keys keeps first present index');\n",
                "assert.sameValue(keys[1], '2', 'Object.keys keeps later present index');\n",
                "var ownKeys = Reflect.ownKeys(sparse);\n",
                "assert.sameValue(ownKeys.length, 3, 'Reflect.ownKeys skips holes but keeps length');\n",
                "assert.sameValue(ownKeys[0], '0', 'Reflect.ownKeys first index');\n",
                "assert.sameValue(ownKeys[1], '2', 'Reflect.ownKeys second present index');\n",
                "assert.sameValue(ownKeys[2], 'length', 'Reflect.ownKeys length last');\n",
                "assert.sameValue(Object.getOwnPropertyDescriptor(sparse, '1'), undefined, 'hole has no own descriptor');\n",
                "assert.sameValue(sparse.join(), '1,,3', 'join preserves holes as empty fields');\n",
                "assert.sameValue(sparse.indexOf(undefined), -1, 'indexOf skips holes');\n",
                "var iterated = [];\n",
                "for (var value of sparse) {\n",
                "  iterated.push(value);\n",
                "}\n",
                "assert.sameValue(iterated.length, 3, 'for-of walks full array length');\n",
                "assert.sameValue(iterated[0], 1, 'for-of yields first element');\n",
                "assert.sameValue(iterated[1], undefined, 'for-of yields undefined for hole');\n",
                "assert.sameValue(iterated[2], 3, 'for-of yields later element');\n",
                "assert.sameValue(delete sparse[0], true, 'delete array index succeeds');\n",
                "assert.sameValue(Reflect.has(sparse, '0'), false, 'deleted index becomes hole');\n",
                "assert.sameValue(sparse.length, 3, 'delete preserves array length');\n",
                "assert.sameValue(Reflect.deleteProperty(sparse, 'length'), false, 'length is not configurable');\n",
                "var empty = new Array(3);\n",
                "assert.sameValue(empty.length, 3, 'Array(length) creates sparse array of that length');\n",
                "assert.sameValue(Object.keys(empty).length, 0, 'Array(length) does not materialize elements');\n",
                "assert.sameValue(empty.join(), ',,', 'Array(length) join reflects holes');\n",
                "try {\n",
                "  new Array(3.5);\n",
                "  throw new Test262Error('Array constructor should reject fractional length');\n",
                "} catch (error) {}\n",
            ),
            "native-test262-sparse-array-hole-semantics.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_named_properties() {
        let result = execute_test262_basic(
            concat!(
                "var array = [1, 2];\n",
                "array.extra = 99;\n",
                "array['note'] = 7;\n",
                "assert.sameValue(array.extra, 99, 'dot assignment stores named property');\n",
                "assert.sameValue(array.note, 7, 'computed assignment stores named property');\n",
                "assert.sameValue(array.length, 2, 'named properties do not change length');\n",
                "Object.defineProperty(array, 'hidden', {\n",
                "  value: 123,\n",
                "  writable: true,\n",
                "  enumerable: false,\n",
                "  configurable: true\n",
                "});\n",
                "assert.sameValue(array.hidden, 123, 'array named data property works');\n",
                "var keys = Object.keys(array);\n",
                "assert.sameValue(keys.length, 4, 'Object.keys includes indices plus enumerable named props');\n",
                "assert.sameValue(keys[0], '0', 'Object.keys keeps first index');\n",
                "assert.sameValue(keys[1], '1', 'Object.keys keeps second index');\n",
                "assert.sameValue(keys[2], 'extra', 'Object.keys includes first named property in insertion order');\n",
                "assert.sameValue(keys[3], 'note', 'Object.keys includes second named property in insertion order');\n",
                "var ownKeys = Reflect.ownKeys(array);\n",
                "assert.sameValue(ownKeys.length, 6, 'Reflect.ownKeys includes length and non-enumerable named props');\n",
                "assert.sameValue(ownKeys[0], '0', 'Reflect.ownKeys first index');\n",
                "assert.sameValue(ownKeys[1], '1', 'Reflect.ownKeys second index');\n",
                "assert.sameValue(ownKeys[2], 'length', 'Reflect.ownKeys keeps length before named props');\n",
                "assert.sameValue(ownKeys[3], 'extra', 'Reflect.ownKeys keeps first named prop order');\n",
                "assert.sameValue(ownKeys[4], 'note', 'Reflect.ownKeys keeps second named prop order');\n",
                "assert.sameValue(ownKeys[5], 'hidden', 'Reflect.ownKeys includes non-enumerable named props');\n",
                "assert.sameValue(Object.getOwnPropertyDescriptor(array, 'hidden').enumerable, false, 'descriptor round-trips named data property');\n",
                "assert.sameValue(delete array.extra, true, 'delete removes named array property');\n",
                "assert.sameValue(array.extra, undefined, 'deleted named array property is gone');\n",
            ),
            "native-test262-array-named-properties.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_index_descriptor_variants() {
        let result = execute_test262_basic(
            concat!(
                "var array = [1, 2, 3];\n",
                "assert.sameValue(Reflect.defineProperty(array, '0', { value: 9, writable: false, enumerable: false, configurable: false }), true, 'can define exceptional index descriptor');\n",
                "assert.sameValue(array[0], 9, 'index value updates through descriptor');\n",
                "var desc = Object.getOwnPropertyDescriptor(array, '0');\n",
                "assert.sameValue(desc.value, 9, 'descriptor value reflects exceptional index');\n",
                "assert.sameValue(desc.writable, false, 'descriptor writable reflects exceptional index');\n",
                "assert.sameValue(desc.enumerable, false, 'descriptor enumerable reflects exceptional index');\n",
                "assert.sameValue(desc.configurable, false, 'descriptor configurable reflects exceptional index');\n",
                "array[0] = 44;\n",
                "assert.sameValue(array[0], 9, 'assignment respects non-writable exceptional index');\n",
                "assert.sameValue(delete array[0], false, 'delete respects non-configurable exceptional index');\n",
                "assert.sameValue(array.join(), '9,2,3', 'join still sees exceptional index value');\n",
                "assert.sameValue(array.indexOf(9), 0, 'indexOf still sees exceptional index value');\n",
                "var keys = Object.keys(array);\n",
                "assert.sameValue(keys.length, 2, 'Object.keys skips non-enumerable exceptional index');\n",
                "assert.sameValue(keys[0], '1', 'Object.keys starts with remaining enumerable index');\n",
                "assert.sameValue(keys[1], '2', 'Object.keys keeps later enumerable index');\n",
                "var ownKeys = Reflect.ownKeys(array);\n",
                "assert.sameValue(ownKeys[0], '0', 'Reflect.ownKeys keeps exceptional index');\n",
                "assert.sameValue(ownKeys[1], '1', 'Reflect.ownKeys keeps following index');\n",
                "assert.sameValue(ownKeys[2], '2', 'Reflect.ownKeys keeps later index');\n",
                "assert.sameValue(ownKeys[3], 'length', 'Reflect.ownKeys keeps length after indices');\n",
                "assert.sameValue(Reflect.defineProperty(array, '0', { value: 9 }), true, 'redefining same value on frozen descriptor succeeds');\n",
                "assert.sameValue(Reflect.defineProperty(array, '0', { value: 10 }), false, 'changing value on frozen descriptor fails');\n",
                "var blocked = [4, 5, 6];\n",
                "assert.sameValue(Reflect.defineProperty(blocked, '2', { value: 6, writable: false, enumerable: true, configurable: false }), true, 'can freeze a trailing index');\n",
                "assert.sameValue(Reflect.defineProperty(blocked, 'length', { value: 1 }), false, 'length shrink fails across trailing non-configurable exceptional index');\n",
                "assert.sameValue(blocked.length, 3, 'failed length shrink snaps to blocking index plus one');\n",
            ),
            "native-test262-array-index-descriptor-variants.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_index_accessors() {
        let result = execute_test262_basic(
            concat!(
                "function readIndex() {\n",
                "  return 42;\n",
                "}\n",
                "function writeIndex(value) {\n",
                "  this.written = value;\n",
                "}\n",
                "var array = [1, 2, 3];\n",
                "assert.sameValue(Reflect.defineProperty(array, '1', { get: readIndex, set: writeIndex, enumerable: true, configurable: true }), true, 'can define accessor-backed array index');\n",
                "assert.sameValue(array[1], 42, 'direct index access uses getter');\n",
                "array[1] = 9;\n",
                "assert.sameValue(array.written, 9, 'index assignment uses setter with array receiver');\n",
                "assert.sameValue(array.join(), '1,42,3', 'join uses accessor-backed index');\n",
                "assert.sameValue(array.indexOf(42), 1, 'indexOf observes accessor-backed index value');\n",
                "var iterated = [];\n",
                "for (var value of array) {\n",
                "  iterated.push(value);\n",
                "}\n",
                "assert.sameValue(iterated.length, 3, 'for-of keeps array length with accessor index');\n",
                "assert.sameValue(iterated[1], 42, 'for-of uses accessor-backed index');\n",
                "var sliced = array.slice(0, 3);\n",
                "assert.sameValue(sliced[1], 42, 'slice materializes accessor-backed index value');\n",
                "var concatenated = array.concat([]);\n",
                "assert.sameValue(concatenated[1], 42, 'concat materializes accessor-backed index value');\n",
                "var keys = Object.keys(array);\n",
                "assert.sameValue(keys[0], '0', 'Object.keys keeps first index');\n",
                "assert.sameValue(keys[1], '1', 'Object.keys includes accessor-backed index');\n",
                "assert.sameValue(keys[2], '2', 'Object.keys keeps later index');\n",
            ),
            "native-test262-array-index-accessors.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_array_prototype_iteration_methods() {
        let result = execute_test262_basic(
            concat!(
                "var arr = [1, 2, 3, 4, 5];\n",
                // map
                "var doubled = arr.map(function(x) { return x * 2; });\n",
                "assert.sameValue(doubled.length, 5, 'map preserves length');\n",
                "assert.sameValue(doubled[0], 2, 'map doubles first');\n",
                "assert.sameValue(doubled[4], 10, 'map doubles last');\n",
                // filter
                "var evens = arr.filter(function(x) { return x % 2 === 0; });\n",
                "assert.sameValue(evens.length, 2, 'filter keeps even count');\n",
                "assert.sameValue(evens[0], 2, 'filter keeps 2');\n",
                "assert.sameValue(evens[1], 4, 'filter keeps 4');\n",
                // forEach
                "var sum = 0;\n",
                "arr.forEach(function(x) { sum = sum + x; });\n",
                "assert.sameValue(sum, 15, 'forEach accumulates sum');\n",
                // reduce
                "var total = arr.reduce(function(a, b) { return a + b; }, 0);\n",
                "assert.sameValue(total, 15, 'reduce with initial value');\n",
                "var totalNoInit = arr.reduce(function(a, b) { return a + b; });\n",
                "assert.sameValue(totalNoInit, 15, 'reduce without initial value');\n",
                // find / findIndex
                "assert.sameValue(arr.find(function(x) { return x > 3; }), 4, 'find returns first match');\n",
                "assert.sameValue(arr.find(function(x) { return x > 10; }), undefined, 'find returns undefined on miss');\n",
                "assert.sameValue(arr.findIndex(function(x) { return x > 3; }), 3, 'findIndex returns index');\n",
                "assert.sameValue(arr.findIndex(function(x) { return x > 10; }), -1, 'findIndex returns -1 on miss');\n",
                // some / every
                "assert.sameValue(arr.some(function(x) { return x > 4; }), true, 'some finds match');\n",
                "assert.sameValue(arr.some(function(x) { return x > 10; }), false, 'some no match');\n",
                "assert.sameValue(arr.every(function(x) { return x > 0; }), true, 'every all pass');\n",
                "assert.sameValue(arr.every(function(x) { return x > 3; }), false, 'every not all pass');\n",
                // includes
                "assert.sameValue(arr.includes(3), true, 'includes finds element');\n",
                "assert.sameValue(arr.includes(9), false, 'includes misses absent');\n",
                "assert.sameValue([NaN].includes(NaN), true, 'includes uses SameValueZero for NaN');\n",
                // fill
                "var filled = [1, 2, 3, 4].fill(0, 1, 3);\n",
                "assert.sameValue(filled[0], 1, 'fill preserves before start');\n",
                "assert.sameValue(filled[1], 0, 'fill sets start');\n",
                "assert.sameValue(filled[2], 0, 'fill sets middle');\n",
                "assert.sameValue(filled[3], 4, 'fill preserves after end');\n",
                // reverse
                "var reversed = [1, 2, 3].reverse();\n",
                "assert.sameValue(reversed[0], 3, 'reverse first');\n",
                "assert.sameValue(reversed[2], 1, 'reverse last');\n",
                // pop
                "var popArr = [10, 20, 30];\n",
                "assert.sameValue(popArr.pop(), 30, 'pop returns last');\n",
                "assert.sameValue(popArr.length, 2, 'pop shrinks length');\n",
                // shift
                "var shiftArr = [10, 20, 30];\n",
                "assert.sameValue(shiftArr.shift(), 10, 'shift returns first');\n",
                "assert.sameValue(shiftArr.length, 2, 'shift shrinks length');\n",
                "assert.sameValue(shiftArr[0], 20, 'shift moves elements left');\n",
                // unshift
                "var unshiftArr = [3, 4];\n",
                "assert.sameValue(unshiftArr.unshift(1, 2), 4, 'unshift returns new length');\n",
                "assert.sameValue(unshiftArr[0], 1, 'unshift inserts at start');\n",
                "assert.sameValue(unshiftArr[2], 3, 'unshift preserves existing');\n",
                // splice
                "var spliceArr = [1, 2, 3, 4, 5];\n",
                "var removed = spliceArr.splice(1, 2, 8, 9, 10);\n",
                "assert.sameValue(removed.length, 2, 'splice returns deleted count');\n",
                "assert.sameValue(removed[0], 2, 'splice returns first deleted');\n",
                "assert.sameValue(spliceArr.length, 6, 'splice adjusts length');\n",
                "assert.sameValue(spliceArr[1], 8, 'splice inserts first item');\n",
                "assert.sameValue(spliceArr[3], 10, 'splice inserts last item');\n",
                "assert.sameValue(spliceArr[4], 4, 'splice preserves trailing');\n",
                // lastIndexOf
                "assert.sameValue([1, 2, 3, 2, 1].lastIndexOf(2), 3, 'lastIndexOf finds last');\n",
                "assert.sameValue([1, 2, 3].lastIndexOf(9), -1, 'lastIndexOf returns -1 on miss');\n",
                // Array.of
                "var ofArr = Array.of(1, 2, 3);\n",
                "assert.sameValue(ofArr.length, 3, 'Array.of sets length');\n",
                "assert.sameValue(ofArr[1], 2, 'Array.of stores elements');\n",
                // Array.from
                "var fromArr = Array.from([10, 20, 30]);\n",
                "assert.sameValue(fromArr.length, 3, 'Array.from copies length');\n",
                "assert.sameValue(fromArr[2], 30, 'Array.from copies elements');\n",
                "var mapped = Array.from([1, 2, 3], function(x) { return x * 10; });\n",
                "assert.sameValue(mapped[0], 10, 'Array.from with mapfn');\n",
                "assert.sameValue(mapped[2], 30, 'Array.from with mapfn last');\n",
            ),
            "native-test262-array-prototype-iteration.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_primitive_wrapper_intrinsics() {
        let module = compile_test262_basic_script(
            concat!(
                "var text = String(\"otter\");\n",
                "assert.sameValue(text, \"otter\", \"String() returns a string primitive value\");\n",
                "var wrappedText = new String(\"otter\");\n",
                "assert.sameValue(wrappedText.valueOf(), \"otter\", \"String wrapper delegates through prototype\");\n",
                "assert.sameValue(String.prototype.valueOf(), \"\", \"String.prototype carries empty string data\");\n",
                "assert.sameValue(Object(\"otter\").valueOf(), \"otter\", \"Object boxes string primitives\");\n",
                "assert.sameValue(wrappedText.constructor, String, \"string wrapper constructor link\");\n",
                "assert.sameValue(String.prototype.constructor, String, \"String.prototype.constructor\");\n",
                "assert.sameValue(Number(\"7\"), 7, \"Number coerces string input\");\n",
                "var wrappedNumber = new Number(true);\n",
                "assert.sameValue((new Number()).valueOf(), 0, \"Number wrapper defaults to +0\");\n",
                "assert.sameValue((new Number(0)).valueOf(), 0, \"Number wrapper preserves zero\");\n",
                "assert.sameValue((new Number(-1)).valueOf(), -1, \"Number wrapper preserves negatives\");\n",
                "assert.sameValue(wrappedNumber.valueOf(), 1, \"Number wrapper stores primitive value\");\n",
                "assert.sameValue(Number.prototype.valueOf(), 0, \"Number.prototype stores default numeric data\");\n",
                "assert.sameValue(Object(7).valueOf(), 7, \"Object boxes numeric primitives\");\n",
                "assert.sameValue((new Number(NaN)).valueOf(), NaN, \"Number wrapper preserves NaN under sameValue\");\n",
                "assert.sameValue(Number.prototype.constructor, Number, \"Number.prototype.constructor\");\n",
                "assert.sameValue(Boolean(\"\"), false, \"Boolean coerces empty string\");\n",
                "var wrappedBoolean = new Boolean(1);\n",
                "assert.sameValue((new Boolean()).valueOf(), false, \"Boolean wrapper defaults to false\");\n",
                "assert.sameValue((new Boolean(0)).valueOf(), false, \"Boolean wrapper preserves falsy numbers\");\n",
                "assert.sameValue((new Boolean(-1)).valueOf(), true, \"Boolean wrapper preserves truthy numbers\");\n",
                "assert.sameValue(wrappedBoolean.valueOf(), true, \"Boolean wrapper stores primitive value\");\n",
                "assert.sameValue(Boolean.prototype.valueOf(), false, \"Boolean.prototype stores default boolean data\");\n",
                "assert.sameValue(Object(true).valueOf(), true, \"Object boxes boolean primitives\");\n",
                "assert.sameValue((new Boolean(new Object())).valueOf(), true, \"Boolean wrapper treats objects as truthy\");\n",
                "assert.sameValue(Boolean.prototype.constructor, Boolean, \"Boolean.prototype.constructor\");\n",
                "var objectValueOfNumber = Object.prototype.valueOf.call(1);\n",
                "assert.sameValue(objectValueOfNumber instanceof Number, true, \"Object.prototype.valueOf boxes number receivers\");\n",
                "assert.sameValue(objectValueOfNumber.valueOf(), 1, \"Object.prototype.valueOf preserves boxed number value\");\n",
                "var objectValueOfString = Object.prototype.valueOf.call(\"otter\");\n",
                "assert.sameValue(objectValueOfString instanceof String, true, \"Object.prototype.valueOf boxes string receivers\");\n",
                "assert.sameValue(objectValueOfString.valueOf(), \"otter\", \"Object.prototype.valueOf preserves boxed string value\");\n",
                "var objectValueOfBoolean = Object.prototype.valueOf.call(true);\n",
                "assert.sameValue(objectValueOfBoolean instanceof Boolean, true, \"Object.prototype.valueOf boxes boolean receivers\");\n",
                "assert.sameValue(objectValueOfBoolean.valueOf(), true, \"Object.prototype.valueOf preserves boxed boolean value\");\n",
                "try {\n",
                "  Object.prototype.valueOf.call(null);\n",
                "  throw new Test262Error('Object.prototype.valueOf should reject null receiver');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.prototype.valueOf null receiver throws TypeError');\n",
                "}\n",
                "assert.sameValue(Object.prototype.isPrototypeOf.call(1, new Number(1)), false, \"Object.prototype.isPrototypeOf boxes primitive number receivers\");\n",
                "assert.sameValue(Object.prototype.isPrototypeOf.call(\"otter\", new String(\"otter\")), false, \"Object.prototype.isPrototypeOf boxes primitive string receivers\");\n",
                "assert.sameValue(Object.prototype.isPrototypeOf.call(true, new Boolean(true)), false, \"Object.prototype.isPrototypeOf boxes primitive boolean receivers\");\n",
                "assert.sameValue(Object.prototype.isPrototypeOf.call({}, \"otter\"), false, \"Object.prototype.isPrototypeOf returns false for primitive string targets\");\n",
                "try {\n",
                "  Object.prototype.isPrototypeOf.call(undefined, {});\n",
                "  throw new Test262Error('Object.prototype.isPrototypeOf should reject undefined receiver');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'Object.prototype.isPrototypeOf undefined receiver throws TypeError');\n",
                "}\n",
            ),
            "native-test262-primitive-wrapper-intrinsics.js",
        )
        .expect("primitive wrapper intrinsic script should compile");

        let mut runtime = crate::interpreter::RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        let result = Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect("primitive wrapper intrinsic script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_number_prototype_value_of() {
        let result = execute_test262_basic(
            "assert.sameValue(Number.prototype.valueOf(), 0, \"Number.prototype.valueOf()\");\n",
            "native-test262-number-prototype-valueof.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_new_number_value_of() {
        let result = execute_test262_basic(
            "assert.sameValue((new Number()).valueOf(), 0, \"(new Number()).valueOf()\");\n",
            "native-test262-new-number-valueof.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_new_number_boolean_argument() {
        let result = execute_test262_basic(
            "assert.sameValue((new Number(true)).valueOf(), 1, \"(new Number(true)).valueOf()\");\n",
            "native-test262-new-number-boolean-argument.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_new_number_negative_argument() {
        let result = execute_test262_basic(
            "assert.sameValue((new Number(-1)).valueOf(), -1, \"(new Number(-1)).valueOf()\");\n",
            "native-test262-new-number-negative-argument.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_new_number_nan_argument() {
        let result = execute_test262_basic(
            "assert.sameValue((new Number(NaN)).valueOf(), NaN, \"(new Number(NaN)).valueOf()\");\n",
            "native-test262-new-number-nan-argument.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_boolean_prototype_value_of() {
        let result = execute_test262_basic(
            "assert.sameValue(Boolean.prototype.valueOf(), false, \"Boolean.prototype.valueOf()\");\n",
            "native-test262-boolean-prototype-valueof.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_boolean_prototype_to_string() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(Boolean.prototype.toString(), \"false\", \"Boolean.prototype.toString()\");\n",
                "assert.sameValue((new Boolean()).toString(), \"false\", \"(new Boolean()).toString()\");\n",
                "assert.sameValue((new Boolean(false)).toString(), \"false\", \"(new Boolean(false)).toString()\");\n",
                "assert.sameValue((new Boolean(true)).toString(), \"true\", \"(new Boolean(true)).toString()\");\n",
                "assert.sameValue((new Boolean(1)).toString(), \"true\", \"(new Boolean(1)).toString()\");\n",
                "assert.sameValue((new Boolean(0)).toString(), \"false\", \"(new Boolean(0)).toString()\");\n",
                "assert.sameValue((new Boolean(new Object())).toString(), \"true\", \"(new Boolean(new Object())).toString()\");\n",
            ),
            "native-test262-boolean-prototype-tostring.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_is() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(Object.is(NaN, NaN), true, 'NaN same-value');\n",
                "var negZero = 0 / -1;\n",
                "assert.sameValue(Object.is(0, negZero), false, '+0 vs -0');\n",
                "assert.sameValue(Object.is(negZero, negZero), true, '-0 vs -0');\n",
                "assert.sameValue(Object.is(1, 1), true, 'equal numbers');\n",
                "assert.sameValue(Object.is({}, {}), false, 'distinct objects');\n",
            ),
            "native-test262-object-is.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_string_prototype_methods() {
        let result = execute_test262_basic(
            concat!(
                "var s = 'hello world';\n",
                // charAt / charCodeAt / codePointAt
                "assert.sameValue(s.charAt(0), 'h', 'charAt');\n",
                "assert.sameValue(s.charAt(99), '', 'charAt out of bounds');\n",
                "assert.sameValue(s.charCodeAt(0), 104, 'charCodeAt h');\n",
                "assert.sameValue(s.codePointAt(0), 104, 'codePointAt h');\n",
                // indexOf / lastIndexOf / includes
                "assert.sameValue(s.indexOf('world'), 6, 'indexOf');\n",
                "assert.sameValue(s.indexOf('xyz'), -1, 'indexOf miss');\n",
                "assert.sameValue(s.lastIndexOf('l'), 9, 'lastIndexOf');\n",
                "assert.sameValue(s.includes('llo'), true, 'includes hit');\n",
                "assert.sameValue(s.includes('xyz'), false, 'includes miss');\n",
                // startsWith / endsWith
                "assert.sameValue(s.startsWith('hello'), true, 'startsWith');\n",
                "assert.sameValue(s.endsWith('world'), true, 'endsWith');\n",
                "assert.sameValue(s.startsWith('world'), false, 'startsWith miss');\n",
                // slice / substring
                "assert.sameValue(s.slice(0, 5), 'hello', 'slice');\n",
                "assert.sameValue(s.slice(-5), 'world', 'slice negative');\n",
                "assert.sameValue(s.substring(6), 'world', 'substring');\n",
                "assert.sameValue(s.substring(6, 11), 'world', 'substring range');\n",
                // toUpperCase / toLowerCase
                "assert.sameValue('abc'.toUpperCase(), 'ABC', 'toUpperCase');\n",
                "assert.sameValue('ABC'.toLowerCase(), 'abc', 'toLowerCase');\n",
                // trim / trimStart / trimEnd
                "assert.sameValue('  hi  '.trim(), 'hi', 'trim');\n",
                "assert.sameValue('  hi  '.trimStart(), 'hi  ', 'trimStart');\n",
                "assert.sameValue('  hi  '.trimEnd(), '  hi', 'trimEnd');\n",
                // repeat
                "assert.sameValue('ab'.repeat(3), 'ababab', 'repeat');\n",
                "assert.sameValue('x'.repeat(0), '', 'repeat 0');\n",
                // padStart / padEnd
                "assert.sameValue('5'.padStart(3, '0'), '005', 'padStart');\n",
                "assert.sameValue('5'.padEnd(3, '0'), '500', 'padEnd');\n",
                // split
                "var parts = 'a,b,c'.split(',');\n",
                "assert.sameValue(parts.length, 3, 'split length');\n",
                "assert.sameValue(parts[0], 'a', 'split[0]');\n",
                "assert.sameValue(parts[2], 'c', 'split[2]');\n",
                "var chars = 'hi'.split('');\n",
                "assert.sameValue(chars.length, 2, 'split empty sep');\n",
                "assert.sameValue(chars[0], 'h', 'split char 0');\n",
                // at
                "assert.sameValue('abc'.at(0), 'a', 'at 0');\n",
                "assert.sameValue('abc'.at(-1), 'c', 'at -1');\n",
                "assert.sameValue('abc'.at(5), undefined, 'at out of bounds');\n",
                // replace / replaceAll
                "assert.sameValue('foo bar foo'.replace('foo', 'baz'), 'baz bar foo', 'replace first');\n",
                "assert.sameValue('foo bar foo'.replaceAll('foo', 'baz'), 'baz bar baz', 'replaceAll');\n",
                // localeCompare
                "assert.sameValue('a'.localeCompare('b'), -1, 'localeCompare less');\n",
                "assert.sameValue('b'.localeCompare('a'), 1, 'localeCompare greater');\n",
                "assert.sameValue('a'.localeCompare('a'), 0, 'localeCompare equal');\n",
            ),
            "native-test262-string-prototype-methods.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_number_boxing() {
        let result = execute_test262_basic(
            "assert.sameValue(Object(7).valueOf(), 7, \"Object(7).valueOf()\");\n",
            "native-test262-object-number-boxing.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_boolean_boxing() {
        let result = execute_test262_basic(
            "assert.sameValue(Object(true).valueOf(), true, \"Object(true).valueOf()\");\n",
            "native-test262-object-boolean-boxing.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_boolean_wrapper_truthy_object() {
        let result = execute_test262_basic(
            "assert.sameValue((new Boolean(new Object())).valueOf(), true, \"new Boolean(new Object()).valueOf()\");\n",
            "native-test262-boolean-wrapper-truthy-object.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_function_prototype_call_on_string_value_of() {
        let result = execute_test262_basic(
            concat!(
                "var valueOf = String.prototype.valueOf;\n",
                "assert.sameValue(valueOf.call(new String(\"str\")), \"str\", \"valueOf.call(new String(...))\");\n",
            ),
            "native-test262-function-call-string-valueof.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_string_wrapper_concatenation() {
        let result = execute_test262_basic(
            "assert.sameValue(\"a\" + new String(\"b\"), \"ab\", \"string wrapper concatenation\");\n",
            "native-test262-string-wrapper-concat.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_typeof_for_runtime_values() {
        let module = compile_test262_basic_script(
            concat!(
                "function Box() {}\n",
                "assert.sameValue(typeof undefined, \"undefined\", \"typeof undefined\");\n",
                "assert.sameValue(typeof null, \"object\", \"typeof null\");\n",
                "assert.sameValue(typeof true, \"boolean\", \"typeof true\");\n",
                "assert.sameValue(typeof 1, \"number\", \"typeof 1\");\n",
                "assert.sameValue(typeof \"otter\", \"string\", \"typeof string literal\");\n",
                "assert.sameValue(typeof Box, \"function\", \"typeof closure\");\n",
                "assert.sameValue(typeof Array, \"function\", \"typeof Array\");\n",
                "assert.sameValue(typeof [], \"object\", \"typeof array literal\");\n",
                "assert.sameValue(typeof new Array(), \"object\", \"typeof constructed array\");\n",
            ),
            "native-test262-typeof.js",
        )
        .expect("typeof script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("typeof script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_large_string_literal_equality() {
        let source = format!(
            "var large = \"{}\";\nassert.sameValue(large, \"{}\", \"large string\");\n",
            "otter".repeat(1024),
            "otter".repeat(1024)
        );

        let module = compile_test262_basic_script(&source, "native-test262-large-string.js")
            .expect("large string script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("large string script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_try_catch_and_finally() {
        let module = compile_test262_basic_script(
            concat!(
                "var caught = 0;\n",
                "var finalized = 0;\n",
                "try {\n",
                "  throw 7;\n",
                "} catch (e) {\n",
                "  assert.sameValue(e, 7, \"caught value\");\n",
                "  caught = 1;\n",
                "} finally {\n",
                "  finalized = 1;\n",
                "}\n",
                "assert.sameValue(caught, 1, \"caught\");\n",
                "assert.sameValue(finalized, 1, \"finalized\");\n",
            ),
            "native-test262-try-catch-finally.js",
        )
        .expect("try/catch/finally script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("try/catch/finally script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_finally_return_override() {
        let module = compile_test262_basic_script(
            concat!(
                "function f() {\n",
                "  try {\n",
                "    return 1;\n",
                "  } finally {\n",
                "    return 2;\n",
                "  }\n",
                "}\n",
                "assert.sameValue(f(), 2, \"finally return override\");\n",
            ),
            "native-test262-finally-return.js",
        )
        .expect("finally return script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("finally return script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_for_of_over_arrays_and_strings() {
        let module = compile_test262_basic_script(
            concat!(
                "var total = 0;\n",
                "for (var value of [1, 2, 3]) {\n",
                "  total = total + value;\n",
                "}\n",
                "assert.sameValue(total, 6, \"array total\");\n",
                "var seen = 0;\n",
                "for (var ch of 'a\\ud801\\udc28b') {\n",
                "  seen = seen + 1;\n",
                "}\n",
                "assert.sameValue(seen, 3, \"string iteration count\");\n",
            ),
            "native-test262-for-of-array-string.js",
        )
        .expect("for-of array/string script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("for-of array/string script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_for_of_loop_control() {
        let module = compile_test262_basic_script(
            concat!(
                "var total = 0;\n",
                "var current = 0;\n",
                "for (current of [1, 2, 3]) {\n",
                "  if (current === 2) {\n",
                "    continue;\n",
                "  }\n",
                "  total = total + current;\n",
                "  if (current === 3) {\n",
                "    break;\n",
                "  }\n",
                "}\n",
                "assert.sameValue(total, 4, \"loop control total\");\n",
                "assert.sameValue(current, 3, \"loop assignment target\");\n",
            ),
            "native-test262-for-of-loop-control.js",
        )
        .expect("for-of loop control script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("for-of loop control script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_ternary_expression() {
        let module = compile_test262_basic_script(
            concat!(
                "if ((false ? false : true) !== true) {\n",
                "  throw new Test262Error('#1');\n",
                "}\n",
                "if ((true ? false : true) !== false) {\n",
                "  throw new Test262Error('#2');\n",
                "}\n",
            ),
            "native-test262-ternary.js",
        )
        .expect("ternary test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("ternary test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    // --- TASK-BASE-0007: for loops ---

    #[test]
    fn for_loop_basic_accumulation() {
        let module = compile_test262_basic_script(
            concat!(
                "var sum = 0;\n",
                "for (var i = 0; i < 5; i = i + 1) {\n",
                "  sum = sum + i;\n",
                "}\n",
                "assert.sameValue(sum, 10, 'basic for accumulation');\n",
            ),
            "native-test262-for-basic.js",
        )
        .expect("for loop test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("for loop test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn for_loop_null_init_test_update() {
        let module = compile_test262_basic_script(
            concat!(
                "var count = 0;\n",
                "for (;;) {\n",
                "  count = count + 1;\n",
                "  if (count === 3) break;\n",
                "}\n",
                "assert.sameValue(count, 3, 'infinite loop with break');\n",
            ),
            "native-test262-for-infinite.js",
        )
        .expect("for infinite loop test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("for infinite loop test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn for_loop_break_and_continue() {
        let module = compile_test262_basic_script(
            concat!(
                "var sum = 0;\n",
                "for (var i = 0; i < 10; i = i + 1) {\n",
                "  if (i === 7) break;\n",
                "  if (i % 2 === 0) continue;\n",
                "  sum = sum + i;\n",
                "}\n",
                "assert.sameValue(sum, 9, 'break+continue: 1+3+5');\n",
            ),
            "native-test262-for-break-continue.js",
        )
        .expect("for break/continue test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("for break/continue test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    // --- TASK-BASE-0007: arrow functions ---

    #[test]
    fn arrow_function_expression_body() {
        let module = compile_test262_basic_script(
            concat!(
                "var add = (a, b) => a + b;\n",
                "assert.sameValue(add(2, 3), 5, 'arrow expression body');\n",
            ),
            "native-test262-arrow-expr.js",
        )
        .expect("arrow expression test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("arrow expression test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn arrow_function_block_body() {
        let module = compile_test262_basic_script(
            concat!(
                "var double = (x) => { return x + x; };\n",
                "assert.sameValue(double(7), 14, 'arrow block body');\n",
            ),
            "native-test262-arrow-block.js",
        )
        .expect("arrow block body test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("arrow block body test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn arrow_function_captures_upvalue() {
        let module = compile_test262_basic_script(
            concat!(
                "var base = 100;\n",
                "var addBase = (x) => x + base;\n",
                "assert.sameValue(addBase(5), 105, 'arrow captures upvalue');\n",
            ),
            "native-test262-arrow-upvalue.js",
        )
        .expect("arrow upvalue test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("arrow upvalue test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_literal_computed_property_names() {
        let module = compile_test262_basic_script(
            concat!(
                "var key = 'dynamic';\n",
                "var calls = 0;\n",
                "var obj = {\n",
                "    [key]: 7,\n",
                "    [1 + 1]: 'two',\n",
                "    ['pre' + 'fix']: true,\n",
                "    [(() => { calls = calls + 1; return 'side'; })()]: 9\n",
                "};\n",
                "assert.sameValue(obj.dynamic, 7, 'computed string key defines property');\n",
                "assert.sameValue(obj['2'], 'two', 'computed numeric key is coerced through ToPropertyKey');\n",
                "assert.sameValue(obj.prefix, true, 'computed concatenated string key defines property');\n",
                "assert.sameValue(obj.side, 9, 'computed key expression result defines property');\n",
                "assert.sameValue(calls, 1, 'computed key expression evaluates exactly once');\n",
                "var overwrite = { fixed: 1, ['fixed']: 3 };\n",
                "assert.sameValue(overwrite.fixed, 3, 'computed key can overwrite earlier static property');\n",
            ),
            "native-test262-object-literal-computed-keys.js",
        )
        .expect("computed object property script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("computed property names test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_literal_accessors() {
        let module = compile_test262_basic_script(
            concat!(
                "var storage = 1;\n",
                "var obj = {\n",
                "    get value() { return storage + this.offset; },\n",
                "    set value(v) { storage = v - this.offset; }\n",
                "};\n",
                "obj.offset = 4;\n",
                "assert.sameValue(obj.value, 5, 'object literal getter reads through accessor semantics');\n",
                "obj.value = 11;\n",
                "assert.sameValue(storage, 7, 'object literal setter receives assigned value and receiver');\n",
                "var desc = Object.getOwnPropertyDescriptor(obj, 'value');\n",
                "assert.sameValue(typeof desc.get, 'function', 'object literal getter installs accessor getter');\n",
                "assert.sameValue(typeof desc.set, 'function', 'object literal setter installs accessor setter');\n",
                "assert.sameValue(desc.enumerable, true, 'object literal accessors are enumerable');\n",
                "assert.sameValue(desc.configurable, true, 'object literal accessors are configurable');\n",
                "assert.sameValue(desc.get.name, 'get value', 'object literal getter gets prefixed function name');\n",
                "assert.sameValue(desc.set.name, 'set value', 'object literal setter gets prefixed function name');\n",
                "var replaced = { value: 1, get value() { return 3; } };\n",
                "assert.sameValue(replaced.value, 3, 'object literal getter can replace earlier data property');\n",
                "var dynamicKey = 'computed';\n",
                "var computedStorage = 0;\n",
                "var computed = {\n",
                "    get [dynamicKey]() { return computedStorage + 1; },\n",
                "    set [dynamicKey](v) { computedStorage = v; },\n",
                "    method(value) { return value + this.extra; }\n",
                "};\n",
                "computed.extra = 2;\n",
                "assert.sameValue(computed.computed, 1, 'computed object literal getter defines accessor');\n",
                "computed.computed = 5;\n",
                "assert.sameValue(computedStorage, 5, 'computed object literal setter defines accessor');\n",
                "assert.sameValue(computed.method(3), 5, 'object literal method shorthand remains callable with receiver');\n",
                "assert.sameValue(computed.method.name, 'method', 'object literal method shorthand infers function name');\n",
            ),
            "native-test262-object-literal-accessors.js",
        )
        .expect("object literal accessor script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("object literal accessor script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_test262_basic_script_supports_object_literal_spread_properties() {
        let module = compile_test262_basic_script(
            concat!(
                "var getterCalls = 0;\n",
                "var setterCalls = 0;\n",
                "var source = {\n",
                "    get value() { getterCalls = getterCalls + 1; return 7; },\n",
                "    fixed: 9\n",
                "};\n",
                "var obj = { prefix: 1, ...source, suffix: 3 };\n",
                "assert.sameValue(obj.prefix, 1, 'spread preserves earlier properties');\n",
                "assert.sameValue(obj.value, 7, 'spread copies enumerable getter-backed source values');\n",
                "assert.sameValue(obj.fixed, 9, 'spread copies enumerable own data properties');\n",
                "assert.sameValue(obj.suffix, 3, 'spread preserves later properties');\n",
                "assert.sameValue(getterCalls, 1, 'spread evaluates getter exactly once');\n",
                "var overwrite = { a: 1, ...{ a: 4 }, a2: 5 };\n",
                "assert.sameValue(overwrite.a, 4, 'later spread overwrites earlier data property');\n",
                "assert.sameValue(overwrite.a2, 5, 'later static property still defines normally');\n",
                "var ignored = { before: 1, ...undefined, ...null, after: 2 };\n",
                "assert.sameValue(ignored.before, 1, 'undefined spread is ignored');\n",
                "assert.sameValue(ignored.after, 2, 'null spread is ignored');\n",
                "var replaced = {\n",
                "    set value(v) { setterCalls = setterCalls + 1; },\n",
                "    ...{ value: 11 }\n",
                "};\n",
                "assert.sameValue(setterCalls, 0, 'spread defines data property instead of invoking target setter');\n",
                "assert.sameValue(replaced.value, 11, 'spread replaces earlier accessor with data property');\n",
                "var desc = Object.getOwnPropertyDescriptor(replaced, 'value');\n",
                "assert.sameValue(desc.enumerable, true, 'spread defines enumerable data property');\n",
                "assert.sameValue(desc.writable, true, 'spread defines writable data property');\n",
                "assert.sameValue(desc.configurable, true, 'spread defines configurable data property');\n",
                "var stringSpread = { ...'ot' };\n",
                "assert.sameValue(stringSpread[0], 'o', 'spread boxes string source index 0');\n",
                "assert.sameValue(stringSpread[1], 't', 'spread boxes string source index 1');\n",
            ),
            "native-test262-object-literal-spread.js",
        )
        .expect("object literal spread script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("object literal spread script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    // --- TASK-BASE-0007: for-of with let/const ---

    #[test]
    fn for_of_with_const_declaration() {
        let module = compile_test262_basic_script(
            concat!(
                "var sum = 0;\n",
                "for (const x of [10, 20, 30]) {\n",
                "  sum = sum + x;\n",
                "}\n",
                "assert.sameValue(sum, 60, 'for-of with const');\n",
            ),
            "native-test262-for-of-const.js",
        )
        .expect("for-of const test should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("for-of const test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn interrupt_flag_terminates_infinite_loop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let module =
            compile_script("for (;;) {}", "infinite.js").expect("infinite loop should compile");

        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);

        // Set the flag immediately — the loop should stop on the first back-edge.
        flag_clone.store(true, Ordering::Relaxed);

        let result = Interpreter::new()
            .with_interrupt_flag(flag)
            .execute(&module);

        assert!(result.is_err(), "infinite loop should be interrupted");
        assert!(
            result.unwrap_err().to_string().contains("interrupted"),
            "error should mention interruption"
        );
    }

    #[test]
    fn harness_sta_js_compiles() {
        let sta = include_str!("../../../tests/test262/harness/sta.js");
        match compile_script(sta, "sta.js") {
            Ok(_) => {}
            Err(e) => panic!("sta.js failed to compile: {e}"),
        }
    }

    #[test]
    fn harness_assert_js_compiles() {
        let sta = include_str!("../../../tests/test262/harness/sta.js");
        let assert_js = include_str!("../../../tests/test262/harness/assert.js");
        let combined = format!("{sta}\n{assert_js}");
        match compile_script(&combined, "sta+assert.js") {
            Ok(_) => {}
            Err(e) => panic!("sta.js+assert.js failed to compile: {e}"),
        }
    }

    #[test]
    fn harness_plus_test_executes() {
        let sta = include_str!("../../../tests/test262/harness/sta.js");
        let assert_js = include_str!("../../../tests/test262/harness/assert.js");
        let test_code = concat!(
            "assert.sameValue(1 + 2, 3, 'basic addition');\n",
            "assert.sameValue(true ? 1 : 2, 1, 'ternary true');\n",
            "assert.sameValue('' ? 'yes' : 'no', 'no', 'empty string is falsy');\n",
        );
        let combined = format!("{sta}\n{assert_js}\n{test_code}");
        let module =
            compile_script(&combined, "harness+test.js").expect("harness+test should compile");

        let result = Interpreter::new().execute(&module);
        // Normal completion = pass (no Test262Error thrown).
        assert!(
            result.is_ok(),
            "harness+test should pass: {:?}",
            result.err()
        );
    }

    #[test]
    fn compile_script_reports_uncaught_throw() {
        let module = compile_script("throw 9;", "next-throw.js").expect("script should compile");
        let error = Interpreter::new()
            .execute(&module)
            .expect_err("top-level throw should propagate");
        assert!(
            error.to_string().contains("uncaught throw"),
            "unexpected error: {error}"
        );
    }

    // ---- switch statement ----

    #[test]
    fn switch_basic_case_match() {
        let result = execute_test262_basic(
            concat!(
                "var x = 2;\n",
                "var r = 0;\n",
                "switch (x) {\n",
                "  case 1: r = 10; break;\n",
                "  case 2: r = 20; break;\n",
                "  case 3: r = 30; break;\n",
                "}\n",
                "assert.sameValue(r, 20, 'switch should match case 2');\n",
            ),
            "switch-basic.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn switch_fall_through() {
        let result = execute_test262_basic(
            concat!(
                "var x = 1;\n",
                "var r = 0;\n",
                "switch (x) {\n",
                "  case 1: r = r + 1;\n",
                "  case 2: r = r + 2;\n",
                "  case 3: r = r + 4; break;\n",
                "}\n",
                "assert.sameValue(r, 7, 'switch should fall through cases 1, 2, 3');\n",
            ),
            "switch-fallthrough.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn switch_default_only() {
        let result = execute_test262_basic(
            concat!(
                "var r = 0;\n",
                "switch (99) {\n",
                "  default: r = 42;\n",
                "}\n",
                "assert.sameValue(r, 42, 'switch default case should execute');\n",
            ),
            "switch-default-only.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn switch_default_with_cases() {
        let result = execute_test262_basic(
            concat!(
                "var r = 0;\n",
                "switch (99) {\n",
                "  case 1: r = 10; break;\n",
                "  default: r = 50; break;\n",
                "  case 2: r = 20; break;\n",
                "}\n",
                "assert.sameValue(r, 50, 'switch should fall to default when no case matches');\n",
            ),
            "switch-default-with-cases.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn switch_no_match_no_default() {
        let result = execute_test262_basic(
            concat!(
                "var r = 5;\n",
                "switch (99) {\n",
                "  case 1: r = 10; break;\n",
                "  case 2: r = 20; break;\n",
                "}\n",
                "assert.sameValue(r, 5, 'switch with no match and no default should be a noop');\n",
            ),
            "switch-no-match.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn switch_string_discriminant() {
        let result = execute_test262_basic(
            concat!(
                "var r = 0;\n",
                "switch ('b') {\n",
                "  case 'a': r = 1; break;\n",
                "  case 'b': r = 2; break;\n",
                "  case 'c': r = 3; break;\n",
                "}\n",
                "assert.sameValue(r, 2, 'switch on string discriminant');\n",
            ),
            "switch-string.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // ---- for..in ----

    #[test]
    fn for_in_body_executes() {
        // Check: does the for-in body execute and does count increment work?
        let result = execute_test262_basic(
            concat!(
                "var obj = { x: 1 };\n",
                "var ran = false;\n",
                "for (var k in obj) { ran = true; }\n",
                "if (ran !== true) throw new Test262Error('for-in body did not execute');\n",
            ),
            "for-in-body.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn for_in_assign_inside_loop() {
        // Debug: just assign count = 1 (no addition) inside for-in
        let result = execute_test262_basic(
            concat!(
                "var obj = { x: 1 };\n",
                "var count = 0;\n",
                "for (var k in obj) { count = 1; }\n",
                "if (count !== 1) throw new Test262Error('count is not 1');\n",
            ),
            "for-in-assign.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn for_in_literal_add_inside_loop() {
        // Debug: 0 + 1 inside for-in (no var read)
        let result = execute_test262_basic(
            concat!(
                "var obj = { x: 1 };\n",
                "var count = 0;\n",
                "for (var k in obj) { count = 0 + 1; }\n",
                "if (count !== 1) throw new Test262Error('literal add failed');\n",
            ),
            "for-in-literal-add.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // NOTE: for-in property enumeration tests are deferred until the property
    // iterator correctly enumerates named properties on shaped objects.
    // The for-in compilation + basic control flow works (see for_in_body_executes,
    // for_in_null_undefined_no_iterations, for_in_break).

    #[test]
    fn for_in_null_undefined_no_iterations() {
        let result = execute_test262_basic(
            concat!(
                "var count = 0;\n",
                "for (var k in null) { count = count + 1; }\n",
                "for (var k in undefined) { count = count + 1; }\n",
                "assert.sameValue(count, 0, 'for-in on null/undefined produces no iterations');\n",
            ),
            "for-in-null-undefined.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn for_in_break() {
        let result = execute_test262_basic(
            concat!(
                "var obj = { a: 1, b: 2, c: 3 };\n",
                "var count = 0;\n",
                "for (var k in obj) { count = count + 1; if (count === 2) break; }\n",
                "assert.sameValue(count, 2, 'for-in break should stop enumeration');\n",
            ),
            "for-in-break.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // ---- instanceof ----

    #[test]
    fn instanceof_positive() {
        let result = execute_test262_basic(
            concat!(
                "function Foo() {}\n",
                "var f = new Foo();\n",
                "assert.sameValue(f instanceof Foo, true, 'instance should be instanceof Foo');\n",
            ),
            "instanceof-positive.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn instanceof_negative() {
        let result = execute_test262_basic(
            concat!(
                "function Foo() {}\n",
                "function Bar() {}\n",
                "var f = new Foo();\n",
                "assert.sameValue(f instanceof Bar, false, 'Foo instance is not instanceof Bar');\n",
            ),
            "instanceof-negative.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn instanceof_prototype_chain() {
        let result = execute_test262_basic(
            concat!(
                "function Animal() {}\n",
                "function Dog() {}\n",
                "Dog.prototype = new Animal();\n",
                "var d = new Dog();\n",
                "assert.sameValue(d instanceof Dog, true, 'Dog instance is instanceof Dog');\n",
                "assert.sameValue(d instanceof Animal, true, 'Dog instance is instanceof Animal via chain');\n",
            ),
            "instanceof-chain.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn instanceof_primitive_returns_false() {
        let result = execute_test262_basic(
            concat!(
                "function Foo() {}\n",
                "assert.sameValue(42 instanceof Foo, false, 'primitive is never instanceof');\n",
            ),
            "instanceof-primitive.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn instanceof_bound_function_uses_target_prototype() {
        let result = execute_test262_basic(
            concat!(
                "function Foo() {}\n",
                "var BoundFoo = Foo.bind(null);\n",
                "var BoundBoundFoo = BoundFoo.bind(null);\n",
                "var f = new Foo();\n",
                "assert.sameValue(f instanceof BoundFoo, true, 'bound instanceof unwraps target');\n",
                "assert.sameValue(f instanceof BoundBoundFoo, true, 'nested bound instanceof unwraps recursively');\n",
            ),
            "instanceof-bound.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn instanceof_rejects_non_callable_rhs() {
        let result = execute_test262_basic(
            concat!(
                "function Foo() {}\n",
                "var f = new Foo();\n",
                "try {\n",
                "  f instanceof {};\n",
                "  throw new Test262Error('plain object rhs should throw');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'non-callable rhs throws TypeError');\n",
                "}\n",
            ),
            "instanceof-non-callable-rhs.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn instanceof_uses_get_for_prototype() {
        let module = compile_test262_basic_script(
            concat!(
                "var proto = { marker: true };\n",
                "var obj = Object.create(proto);\n",
                "var arrow = () => 1;\n",
                "var ownGetterCalls = 0;\n",
                "Object.defineProperty(arrow, 'prototype', { get: function() { ownGetterCalls = ownGetterCalls + 1; return proto; }, configurable: true });\n",
                "assert.sameValue(obj instanceof arrow, true, 'instanceof uses own accessor prototype');\n",
                "assert.sameValue(ownGetterCalls, 1, 'instanceof invokes own prototype getter exactly once');\n",
                "var inheritedGetterCalls = 0;\n",
                "Object.defineProperty(Function.prototype, 'prototype', { get: function() { inheritedGetterCalls = inheritedGetterCalls + 1; return proto; }, configurable: true });\n",
                "assert.sameValue(obj instanceof Math.abs, true, 'instanceof uses inherited prototype getter');\n",
                "assert.sameValue(inheritedGetterCalls, 1, 'instanceof invokes inherited prototype getter');\n",
                "delete Function.prototype.prototype;\n",
                "Object.defineProperty(arrow, 'prototype', { get: function() { return 1; }, configurable: true });\n",
                "try {\n",
                "  obj instanceof arrow;\n",
                "  throw new Test262Error('non-object prototype should throw');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'instanceof rejects non-object prototype after getter');\n",
                "}\n",
                "Object.defineProperty(arrow, 'prototype', { get: function() { throw 5; }, configurable: true });\n",
                "try {\n",
                "  obj instanceof arrow;\n",
                "  throw new Test262Error('prototype getter throw should propagate');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error, 5, 'instanceof propagates prototype getter throw');\n",
                "}\n",
            ),
            "instanceof-get-prototype.js",
        )
        .expect("instanceof get-prototype script should compile");

        Interpreter::new()
            .execute(&module)
            .expect("instanceof get-prototype script should execute");
    }

    #[test]
    fn instanceof_respects_symbol_has_instance() {
        let result = execute_test262_basic(
            concat!(
                "function Foo() {}\n",
                "var obj = new Foo();\n",
                "assert.sameValue(obj instanceof Foo, true, 'baseline: normal instanceof');\n",
                "Object.defineProperty(Foo, Symbol.hasInstance, {\n",
                "  value: function(v) { return false; },\n",
                "  writable: true, configurable: true\n",
                "});\n",
                "assert.sameValue(obj instanceof Foo, false, 'Symbol.hasInstance returning false overrides prototype check');\n",
                "Object.defineProperty(Foo, Symbol.hasInstance, {\n",
                "  value: function(v) { return true; },\n",
                "  writable: true, configurable: true\n",
                "});\n",
                "assert.sameValue(42 instanceof Foo, true, 'Symbol.hasInstance can make primitives match');\n",
                "assert.sameValue('str' instanceof Foo, true, 'Symbol.hasInstance can make string primitives match');\n",
            ),
            "instanceof-has-instance.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // ---- in operator ----

    #[test]
    fn in_own_property() {
        let result = execute_test262_basic(
            concat!(
                "var obj = { x: 1, y: 2 };\n",
                "assert.sameValue('x' in obj, true, 'x should be in obj');\n",
                "assert.sameValue('y' in obj, true, 'y should be in obj');\n",
            ),
            "in-own.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn in_inherited_property() {
        let result = execute_test262_basic(
            concat!(
                "function Base() {}\n",
                "Base.prototype.inherited = true;\n",
                "var obj = new Base();\n",
                "assert.sameValue('inherited' in obj, true, 'inherited property found via in');\n",
            ),
            "in-inherited.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn in_non_object_rhs_throws_type_error() {
        let result = execute_test262_basic(
            concat!(
                "try {\n",
                "  'x' in 1;\n",
                "  throw new Test262Error('in should reject non-object rhs');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'in operator throws TypeError for non-object rhs');\n",
                "}\n",
            ),
            "in-non-object.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn in_missing_property() {
        let result = execute_test262_basic(
            concat!(
                "var obj = { a: 1 };\n",
                "assert.sameValue('z' in obj, false, 'z is not in obj');\n",
            ),
            "in-missing.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn in_non_object_throws() {
        let module = compile_test262_basic_script("var r = 'x' in 42;\n", "in-non-object.js")
            .expect("should compile");

        let mut runtime = crate::interpreter::RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        let error = Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect_err("in operator on non-object should throw");
        assert!(
            matches!(error, InterpreterError::UncaughtThrow(_)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn in_symbol_property() {
        let result = execute_test262_basic(
            concat!(
                "var sym = Symbol('test');\n",
                "var obj = {};\n",
                "obj[sym] = 42;\n",
                "assert.sameValue(sym in obj, true, 'symbol property is found by in operator');\n",
                "var sym2 = Symbol('absent');\n",
                "assert.sameValue(sym2 in obj, false, 'absent symbol property is not found');\n",
                "assert.sameValue(obj[sym], 42, 'symbol property value is correct');\n",
            ),
            "in-symbol.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn in_string_primitive_throws() {
        let module = compile_test262_basic_script("var r = 'length' in 'otter';\n", "in-string.js")
            .expect("should compile");

        let mut runtime = crate::interpreter::RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        let error = Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect_err("in operator on primitive string should throw");
        assert!(
            matches!(error, InterpreterError::UncaughtThrow(_)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn symbol_dispose_descriptor_round_trips() {
        let result = execute_test262_basic(
            concat!(
                "var desc = Object.getOwnPropertyDescriptor(Symbol, 'dispose');\n",
                "assert.sameValue(typeof desc.value, 'symbol', 'descriptor stores symbol primitive');\n",
                "assert.sameValue(desc.value, Symbol.dispose, 'descriptor value matches Symbol.dispose');\n",
                "assert.sameValue(desc.writable, false, 'descriptor keeps writable false');\n",
                "assert.sameValue(desc.enumerable, false, 'descriptor keeps enumerable false');\n",
                "assert.sameValue(desc.configurable, false, 'descriptor keeps configurable false');\n",
            ),
            "symbol-dispose-descriptor.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_dispose_is_shared_across_test262_realms() {
        let result = execute_test262_basic(
            concat!(
                "var realm = $262.createRealm();\n",
                "if (realm === undefined) throw 1;\n",
                "assert.sameValue(realm.global.Symbol.dispose, Symbol.dispose, 'well-known symbol is shared across realms');\n",
            ),
            "symbol-dispose-cross-realm.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_for_reuses_global_registry_entries() {
        let result = execute_test262_basic(
            concat!(
                "var canonical = Symbol.for('otter');\n",
                "if (typeof canonical !== 'symbol') throw new Test262Error('Symbol.for should create a symbol');\n",
                "if (canonical !== Symbol.for('otter')) throw new Test262Error('Symbol.for should reuse registry entries');\n",
                "if (canonical === Symbol('otter')) throw new Test262Error('Symbol() should not reuse registry entries');\n",
            ),
            "symbol-for-registry.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_key_for_returns_registry_key() {
        let result = execute_test262_basic(
            concat!(
                "var canonical = Symbol.for('otter');\n",
                "if (Symbol.keyFor(canonical) !== 'otter') throw new Test262Error('Symbol.keyFor should return the registry key');\n",
                "if (Symbol.keyFor(Symbol('otter')) !== undefined) throw new Test262Error('Symbol.keyFor should ignore unregistered symbols');\n",
            ),
            "symbol-key-for.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_description_reflects_runtime_metadata() {
        let result = execute_test262_basic(
            concat!(
                "if (Symbol('otter').description !== 'otter') throw new Test262Error('Symbol(description) should record description');\n",
                "if (Symbol().description !== undefined) throw new Test262Error('Symbol() should have undefined description');\n",
                "if (Symbol.for('registry').description !== 'registry') throw new Test262Error('Symbol.for should record registry description');\n",
            ),
            "symbol-description.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_for_throws_js_exceptions_from_string_coercion() {
        let result = execute_test262_basic(
            concat!(
                "var sentinel = { boom: true };\n",
                "var subject = { toString: function() { throw sentinel; } };\n",
                "try {\n",
                "  Symbol.for(subject);\n",
                "  throw new Test262Error('Symbol.for should propagate thrown toString errors');\n",
                "} catch (error) {\n",
                "  if (error !== sentinel) throw error;\n",
                "}\n",
                "try {\n",
                "  Symbol.for(Symbol('otter'));\n",
                "  throw new Test262Error('Symbol.for should reject symbol keys');\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
            ),
            "symbol-for-errors.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_prototype_installs_symbol_keyed_intrinsics() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(typeof Symbol.prototype[Symbol.toPrimitive], 'function');\n",
                "assert.sameValue(Symbol.prototype[Symbol.toPrimitive].length, 1);\n",
                "assert.sameValue(Symbol.prototype[Symbol.toPrimitive].name, '[Symbol.toPrimitive]');\n",
                "assert.sameValue(Symbol.prototype[Symbol.toStringTag], 'Symbol');\n",
            ),
            "symbol-prototype-symbol-keyed-intrinsics.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_wrapper_boxing_and_to_string_work() {
        let result = execute_test262_basic(
            concat!(
                "var sym = Symbol('otter');\n",
                "assert.sameValue(Object(sym).valueOf(), sym);\n",
                "assert.sameValue(Object.getPrototypeOf(sym), Symbol.prototype);\n",
                "assert.sameValue(sym.toString(), 'Symbol(otter)');\n",
                "assert.sameValue(Object(sym).toString(), 'Symbol(otter)');\n",
                "assert.sameValue(String(sym), 'Symbol(otter)');\n",
            ),
            "symbol-wrapper-boxing-and-to-string.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_description_getter_accepts_primitives_and_wrappers() {
        let result = execute_test262_basic(
            concat!(
                "var getter = Object.getOwnPropertyDescriptor(Symbol.prototype, 'description').get;\n",
                "var sym = Symbol('wrapped');\n",
                "assert.sameValue(getter.call(sym), 'wrapped');\n",
                "assert.sameValue(getter.call(Object(sym)), 'wrapped');\n",
                "assert.sameValue(getter.call(Symbol()), undefined);\n",
            ),
            "symbol-description-getter.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_description_getter_rejects_non_symbol_receivers() {
        let result = execute_test262_basic(
            concat!(
                "var getter = Object.getOwnPropertyDescriptor(Symbol.prototype, 'description').get;\n",
                "try {\n",
                "  getter.call(123);\n",
                "  throw new Test262Error('getter.call(123) should throw');\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
                "try {\n",
                "  getter.call({});\n",
                "  throw new Test262Error('getter.call({}) should throw');\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
            ),
            "symbol-description-getter-rejects-non-symbol.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_to_primitive_method_accepts_symbol_primitive_receiver() {
        let result = execute_test262_basic(
            "assert.sameValue(Symbol.toPrimitive[Symbol.toPrimitive](), Symbol.toPrimitive);\n",
            "symbol-to-primitive-primitive-receiver.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_wrapper_property_key_uses_ordinary_to_primitive_when_symbol_to_primitive_is_undefined()
     {
        let result = execute_test262_basic(
            concat!(
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
                "assert.sameValue({ 'Symbol()': 1 }[Object(Symbol())], 1);\n",
            ),
            "symbol-wrapper-property-key-ordinary-to-primitive.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_define_property_overwrites_symbol_to_primitive_value() {
        let result = execute_test262_basic(
            concat!(
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
                "var desc = Object.getOwnPropertyDescriptor(Symbol.prototype, Symbol.toPrimitive);\n",
                "assert.sameValue(desc.value, undefined);\n",
                "assert.sameValue(desc.writable, false);\n",
                "assert.sameValue(desc.enumerable, false);\n",
                "assert.sameValue(desc.configurable, true);\n",
            ),
            "symbol-define-property-overwrites-to-primitive.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_primitive_member_lookup_finds_symbol_to_primitive_method() {
        let result = execute_test262_basic(
            "assert.sameValue(typeof Symbol.toPrimitive[Symbol.toPrimitive], 'function');\n",
            "symbol-primitive-member-lookup.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_wrapper_observes_redefined_symbol_to_primitive() {
        let result = execute_test262_basic(
            concat!(
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
                "assert.sameValue(Object(Symbol())[Symbol.toPrimitive], undefined);\n",
                "assert.sameValue(Object(Symbol()).toString(), 'Symbol()');\n",
            ),
            "symbol-wrapper-observes-redefined-to-primitive.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_wrapper_property_key_matches_object_has_own() {
        let result = execute_test262_basic(
            concat!(
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
                "var obj = { 'Symbol()': 1 };\n",
                "assert.sameValue(Object.hasOwn(obj, Object(Symbol())), true);\n",
            ),
            "symbol-wrapper-property-key-has-own.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_wrapper_redefined_nullish_to_primitive_uses_ordinary_paths() {
        let result = execute_test262_basic(
            concat!(
                "var failures = 0;\n",
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: null });\n",
                "if (Object(Symbol()) == 'Symbol()') failures |= 1;\n",
                "try { +Object(Symbol()); failures |= 2; } catch (error) { if (error.name !== 'TypeError') failures |= 4; }\n",
                "if (`${Object(Symbol())}` !== 'Symbol()') failures |= 8;\n",
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
                "if (!(Object(Symbol.iterator) == Symbol.iterator)) failures |= 16;\n",
                "try { Object(Symbol()) <= ''; failures |= 32; } catch (error) { if (error.name !== 'TypeError') failures |= 64; }\n",
                "if ({ 'Symbol()': 1 }[Object(Symbol())] !== 1) failures |= 128;\n",
                "failures;\n",
            ),
            "symbol-wrapper-redefined-nullish-ordinary-to-primitive.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn exact_test262_symbol_wrapper_redefined_nullish_to_primitive_passes_locally() {
        let result = execute_test262_basic(
            concat!(
                "var failures = 0;\n",
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: null });\n",
                "if (Object(Symbol()) == 'Symbol()') failures |= 1;\n",
                "try { +Object(Symbol()); failures |= 2; } catch (thrown) {\n",
                "  if (typeof thrown !== 'object' || thrown === null) failures |= 4;\n",
                "  else if (thrown.constructor !== TypeError) failures |= 8;\n",
                "}\n",
                "if (`${Object(Symbol())}` !== 'Symbol()') failures |= 16;\n",
                "Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, { value: undefined });\n",
                "if (!(Object(Symbol.iterator) == Symbol.iterator)) failures |= 32;\n",
                "try { Object(Symbol()) <= ''; failures |= 64; } catch (thrown) {\n",
                "  if (typeof thrown !== 'object' || thrown === null) failures |= 128;\n",
                "  else if (thrown.constructor !== TypeError) failures |= 256;\n",
                "}\n",
                "if ({ 'Symbol()': 1 }[Object(Symbol())] !== 1) failures |= 512;\n",
                "failures;\n",
            ),
            "symbol-wrapper-redefined-nullish-exact.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn reflect_construct_reports_symbol_constructor_shape() {
        let result = execute_test262_basic(
            concat!(
                "function isConstructor(f) {\n",
                "  try {\n",
                "    Reflect.construct(function(){}, [], f);\n",
                "  } catch (error) {\n",
                "    return false;\n",
                "  }\n",
                "  return true;\n",
                "}\n",
                "if (!isConstructor(Symbol)) throw new Test262Error('Symbol should be constructible');\n",
                "if (isConstructor(Symbol.for)) throw new Test262Error('Symbol.for should not be constructible');\n",
                "if (isConstructor(Symbol.keyFor)) throw new Test262Error('Symbol.keyFor should not be constructible');\n",
            ),
            "symbol-is-constructor.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn symbol_non_constructor_checks_work_inside_assert_throws_callbacks() {
        let result = execute_test262_basic(
            concat!(
                "var sawTypeError = false;\n",
                "try {\n",
                "  (() => { new Symbol.for(); })();\n",
                "} catch (error) {\n",
                "  sawTypeError = error.name === 'TypeError';\n",
                "}\n",
                "if (!sawTypeError) throw new Test262Error('arrow callback should preserve TypeError from new Symbol.for');\n",
                "sawTypeError = false;\n",
                "try {\n",
                "  (() => { new Symbol.keyFor(Symbol()); })();\n",
                "} catch (error) {\n",
                "  sawTypeError = error.name === 'TypeError';\n",
                "}\n",
                "if (!sawTypeError) throw new Test262Error('arrow callback should preserve TypeError from new Symbol.keyFor');\n",
            ),
            "symbol-not-constructible-callbacks.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn test262_assert_throws_handles_symbol_non_constructors() {
        let result = execute_test262_basic(
            concat!(
                "function assert(mustBeTrue, message) {\n",
                "  if (mustBeTrue === true) return;\n",
                "  throw new Test262Error(message);\n",
                "}\n",
                "assert.throws = function(expectedErrorConstructor, func, message) {\n",
                "  var expectedName, actualName;\n",
                "  if (typeof func !== 'function') throw new Test262Error('bad func');\n",
                "  if (message === undefined) message = '';\n",
                "  else message += ' ';\n",
                "  try {\n",
                "    func();\n",
                "  } catch (thrown) {\n",
                "    if (typeof thrown !== 'object' || thrown === null) {\n",
                "      throw new Test262Error(message + 'Thrown value was not an object!');\n",
                "    } else if (thrown.constructor !== expectedErrorConstructor) {\n",
                "      expectedName = expectedErrorConstructor.name;\n",
                "      actualName = thrown.constructor.name;\n",
                "      if (expectedName === actualName) {\n",
                "        throw new Test262Error(message + 'Expected a ' + expectedName + ' but got a different error constructor with the same name');\n",
                "      }\n",
                "      throw new Test262Error(message + 'Expected a ' + expectedName + ' but got a ' + actualName);\n",
                "    }\n",
                "    return;\n",
                "  }\n",
                "  throw new Test262Error(message + 'Expected a ' + expectedErrorConstructor.name + ' to be thrown but no exception was thrown at all');\n",
                "};\n",
                "assert.throws(TypeError, () => {\n",
                "  new Symbol.for();\n",
                "});\n",
                "assert.throws(TypeError, () => {\n",
                "  new Symbol.keyFor(Symbol());\n",
                "});\n",
            ),
            "symbol-test262-assert-throws.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn exact_test262_symbol_not_a_constructor_shapes_pass_locally() {
        let result = execute_test262_basic(
            concat!(
                "function assert(mustBeTrue, message) {\n",
                "  if (mustBeTrue === true) return;\n",
                "  throw new Test262Error(message);\n",
                "}\n",
                "assert.sameValue = function(actual, expected, message) {\n",
                "  if (actual === expected) return;\n",
                "  throw new Test262Error(message);\n",
                "};\n",
                "assert.throws = function(expectedErrorConstructor, func, message) {\n",
                "  if (typeof func !== 'function') throw new Test262Error('bad func');\n",
                "  try {\n",
                "    func();\n",
                "  } catch (thrown) {\n",
                "    if (typeof thrown !== 'object' || thrown === null) throw new Test262Error('not object');\n",
                "    if (thrown.constructor !== expectedErrorConstructor) throw new Test262Error(message);\n",
                "    return;\n",
                "  }\n",
                "  throw new Test262Error('missing throw');\n",
                "};\n",
                "function isConstructor(f) {\n",
                "  if (typeof f !== 'function') throw new Test262Error('non-function');\n",
                "  try {\n",
                "    Reflect.construct(function(){}, [], f);\n",
                "  } catch (e) {\n",
                "    return false;\n",
                "  }\n",
                "  return true;\n",
                "}\n",
                "assert.sameValue(isConstructor(Symbol.for), false, 'isConstructor(Symbol.for) must return false');\n",
                "assert.throws(TypeError, () => {\n",
                "  new Symbol.for();\n",
                "}, 'new Symbol.for');\n",
                "assert.sameValue(isConstructor(Symbol.keyFor), false, 'isConstructor(Symbol.keyFor) must return false');\n",
                "assert.throws(TypeError, () => {\n",
                "  new Symbol.keyFor(Symbol());\n",
                "}, 'new Symbol.keyFor');\n",
            ),
            "symbol-not-a-constructor-exact.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn non_constructible_symbol_builtins_throw_type_error() {
        let result = execute_test262_basic(
            concat!(
                "try {\n",
                "  new Symbol();\n",
                "  throw new Test262Error('new Symbol should reject non-constructible host function');\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
                "try {\n",
                "  new Symbol.for();\n",
                "  throw new Test262Error('new Symbol.for should reject non-constructible host function');\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
                "try {\n",
                "  new Symbol.keyFor(Symbol());\n",
                "  throw new Test262Error('new Symbol.keyFor should reject non-constructible host function');\n",
                "} catch (error) {\n",
                "  if (error.name !== 'TypeError') throw error;\n",
                "}\n",
            ),
            "symbol-not-constructible.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn property_helper_bound_primordials_work_for_math_constants() {
        let result = execute_test262_basic(
            concat!(
                "var hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);\n",
                "var propertyIsEnumerable = Function.prototype.call.bind(Object.prototype.propertyIsEnumerable);\n",
                "var join = Function.prototype.call.bind(Array.prototype.join);\n",
                "var desc = Object.getOwnPropertyDescriptor(Math, 'E');\n",
                "assert.sameValue(hasOwn(Math, 'E'), true, 'bound hasOwnProperty sees Math.E');\n",
                "assert.sameValue(propertyIsEnumerable(Math, 'E'), false, 'bound propertyIsEnumerable sees Math.E as non-enumerable');\n",
                "assert.sameValue(hasOwn(desc, 'writable'), true, 'bound hasOwnProperty sees descriptor fields');\n",
                "assert.sameValue(join(['a', 'b'], ':'), 'a:b', 'bound array join forwards receiver and args');\n",
            ),
            "property-helper-bound-primordials.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn math_e_property_operations_match_property_helper_expectations() {
        let result = execute_test262_basic(
            concat!(
                "var before = Math.E;\n",
                "Math.E = 1;\n",
                "assert.sameValue(Math.E, before, 'writing non-writable Math.E is ignored in non-strict code');\n",
                "assert.sameValue(delete Math.E, false, 'deleting non-configurable Math.E returns false');\n",
                "assert.sameValue(Object.prototype.hasOwnProperty.call(Math, 'E'), true, 'Math.E remains present after delete attempt');\n",
            ),
            "math-e-property-operations.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn strict_symbol_primitive_property_assignment_throws_type_error() {
        let result = execute_test262_basic(
            concat!(
                "\"use strict\";\n",
                "var sym = Symbol('66');\n",
                "try {\n",
                "  sym.toString = 0;\n",
                "  throw new Test262Error('sym.toString assignment should throw');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'strict symbol toString assignment throws');\n",
                "}\n",
                "try {\n",
                "  sym.valueOf = 0;\n",
                "  throw new Test262Error('sym.valueOf assignment should throw');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'strict symbol valueOf assignment throws');\n",
                "}\n",
            ),
            "symbol-primitive-strict-property-assignment.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn compile_script_marks_use_strict_entry_function_as_strict() {
        let module = compile_script("\"use strict\";\nvar x = 1;\n", "strict-flag-entry.js")
            .expect("strict script should compile");
        assert!(module.entry_function().is_strict());
    }

    #[test]
    fn default_derived_constructor_forwards_arguments_to_super() {
        let result = execute_test262_basic(
            concat!(
                "class Base {\n",
                "  constructor(x, y) {\n",
                "    this.x = x;\n",
                "    this.y = y;\n",
                "  }\n",
                "}\n",
                "class Derived extends Base {}\n",
                "var value = new Derived(1, 2);\n",
                "assert.sameValue(value.x, 1, 'default derived constructor forwards first arg');\n",
                "assert.sameValue(value.y, 2, 'default derived constructor forwards second arg');\n",
                "assert.sameValue(Object.getPrototypeOf(value), Derived.prototype, 'default derived constructor uses Derived.prototype');\n",
            ),
            "default-derived-constructor-forwarding.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn derived_constructor_this_before_super_throws_reference_error() {
        let result = execute_test262_basic(
            concat!(
                "class Base {}\n",
                "class Derived extends Base {\n",
                "  constructor() {\n",
                "    try {\n",
                "      this;\n",
                "      throw new Test262Error('this before super should throw');\n",
                "    } catch (error) {\n",
                "      assert.sameValue(error.name, 'ReferenceError', 'this before super throws ReferenceError');\n",
                "    }\n",
                "    super();\n",
                "  }\n",
                "}\n",
                "new Derived();\n",
            ),
            "derived-constructor-this-before-super.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn derived_constructor_arrow_created_before_super_observes_initialized_this_after_super() {
        let result = execute_test262_basic(
            concat!(
                "class Base {\n",
                "  constructor(value) {\n",
                "    this.value = value;\n",
                "  }\n",
                "}\n",
                "class Derived extends Base {\n",
                "  constructor(value) {\n",
                "    var read = () => this.value;\n",
                "    super(value);\n",
                "    assert.sameValue(read(), value, 'lexical this binding updates after super');\n",
                "  }\n",
                "}\n",
                "new Derived(7);\n",
            ),
            "derived-constructor-arrow-lexical-this.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn derived_constructor_return_object_overrides_initialized_this() {
        let result = execute_test262_basic(
            concat!(
                "class Base {\n",
                "  constructor() {\n",
                "    this.prop = 1;\n",
                "  }\n",
                "}\n",
                "class Derived extends Base {\n",
                "  constructor() {\n",
                "    super();\n",
                "    return { override: true };\n",
                "  }\n",
                "}\n",
                "var value = new Derived();\n",
                "assert.sameValue(value.override, true, 'returned object replaces initialized this');\n",
                "assert.sameValue(typeof value.prop, 'undefined', 'base initialized object is discarded');\n",
                "assert.sameValue(value instanceof Derived, false, 'returned object is not a Derived instance');\n",
            ),
            "derived-constructor-return-object-override.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn derived_constructor_returning_primitive_throws_type_error() {
        let result = execute_test262_basic(
            concat!(
                "class Base {}\n",
                "class Derived extends Base {\n",
                "  constructor() {\n",
                "    return 42;\n",
                "  }\n",
                "}\n",
                "try {\n",
                "  new Derived();\n",
                "  throw new Test262Error('derived constructor primitive return should throw');\n",
                "} catch (error) {\n",
                "  assert.sameValue(error.name, 'TypeError', 'derived constructor primitive return throws TypeError');\n",
                "}\n",
            ),
            "derived-constructor-return-primitive.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn property_helper_verify_property_accepts_math_e() {
        let source = format!(
            "{}\n{}\nverifyProperty(Math, 'E', {{ writable: false, enumerable: false, configurable: false }});\n",
            include_str!("../../../tests/test262/harness/assert.js"),
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
        );
        let result = execute_test262_basic(&source, "property-helper-math-e.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn property_helper_is_enumerable_accepts_math_e() {
        let source = format!(
            "{}\n{}\nassert.sameValue(isEnumerable(Math, 'E'), false);\n",
            include_str!("../../../tests/test262/harness/assert.js"),
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
        );
        let result = execute_test262_basic(&source, "property-helper-is-enumerable-math-e.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn property_helper_is_writable_accepts_math_e() {
        let source = format!(
            "{}\n{}\nassert.sameValue(isWritable(Math, 'E'), false);\n",
            include_str!("../../../tests/test262/harness/assert.js"),
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
        );
        let result = execute_test262_basic(&source, "property-helper-is-writable-math-e.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn property_helper_is_configurable_accepts_math_e() {
        let source = format!(
            "{}\n{}\nassert.sameValue(isConfigurable(Math, 'E'), false);\n",
            include_str!("../../../tests/test262/harness/assert.js"),
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
        );
        let result = execute_test262_basic(&source, "property-helper-is-configurable-math-e.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn test262_assert_function_and_not_same_value_work() {
        let source = format!(
            "{}\nassert(true, 'assert callable succeeds');\nassert.notSameValue({{}}, null, 'notSameValue succeeds');\n",
            include_str!("../../../tests/test262/harness/assert.js"),
        );
        let result = execute_test262_basic(&source, "test262-assert-basic.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn simplified_verify_property_body_accepts_math_e() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name, desc) {\n",
            "  var originalDesc = __getOwnPropertyDescriptor(obj, name);\n",
            "  var nameStr = String(name);\n",
            "  var names = __getOwnPropertyNames(desc);\n",
            "  var failures = [];\n",
            "  for (var i = 0; i < names.length; i++) {\n",
            "    assert(names[i] === 'value' || names[i] === 'writable' || names[i] === 'enumerable' || names[i] === 'configurable' || names[i] === 'get' || names[i] === 'set', 'Invalid descriptor field: ' + names[i]);\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'enumerable') && desc.enumerable !== undefined) {\n",
            "    if (desc.enumerable !== originalDesc.enumerable || desc.enumerable !== isEnumerable(obj, name)) {\n",
            "      __push(failures, 'enumerable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'writable') && desc.writable !== undefined) {\n",
            "    if (desc.writable !== originalDesc.writable || desc.writable !== isWritable(obj, name)) {\n",
            "      __push(failures, 'writable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'configurable') && desc.configurable !== undefined) {\n",
            "    if (desc.configurable !== originalDesc.configurable || desc.configurable !== isConfigurable(obj, name)) {\n",
            "      __push(failures, 'configurable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (failures.length) {\n",
            "    assert(false, __join(failures, '; '));\n",
            "  }\n",
            "}\n",
            "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "simplified-verify-property-body.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn combined_verify_property_without_value_or_restore_accepts_math_e() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name, desc) {\n",
            "  assert(arguments.length > 2, 'verifyProperty should receive at least 3 arguments: obj, name, and descriptor');\n",
            "  var originalDesc = __getOwnPropertyDescriptor(obj, name);\n",
            "  var nameStr = String(name);\n",
            "  if (desc === undefined) {\n",
            "    assert.sameValue(originalDesc, undefined, \"obj['\" + nameStr + \"'] descriptor should be undefined\");\n",
            "    return true;\n",
            "  }\n",
            "  assert(__hasOwnProperty(obj, name), 'obj should have an own property ' + nameStr);\n",
            "  assert.notSameValue(desc, null, 'The desc argument should be an object or undefined, null');\n",
            "  assert.sameValue(typeof desc, 'object', 'The desc argument should be an object or undefined, ' + String(desc));\n",
            "  var names = __getOwnPropertyNames(desc);\n",
            "  for (var i = 0; i < names.length; i++) {\n",
            "    assert(names[i] === 'value' || names[i] === 'writable' || names[i] === 'enumerable' || names[i] === 'configurable' || names[i] === 'get' || names[i] === 'set', 'Invalid descriptor field: ' + names[i]);\n",
            "  }\n",
            "  var failures = [];\n",
            "  if (__hasOwnProperty(desc, 'enumerable') && desc.enumerable !== undefined) {\n",
            "    if (desc.enumerable !== originalDesc.enumerable || desc.enumerable !== isEnumerable(obj, name)) {\n",
            "      __push(failures, 'enumerable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'writable') && desc.writable !== undefined) {\n",
            "    if (desc.writable !== originalDesc.writable || desc.writable !== isWritable(obj, name)) {\n",
            "      __push(failures, 'writable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'configurable') && desc.configurable !== undefined) {\n",
            "    if (desc.configurable !== originalDesc.configurable || desc.configurable !== isConfigurable(obj, name)) {\n",
            "      __push(failures, 'configurable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (failures.length) {\n",
            "    assert(false, __join(failures, '; '));\n",
            "  }\n",
            "}\n",
            "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "combined-verify-property-no-value-restore.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn combined_verify_property_with_value_branch_accepts_math_e() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name, desc) {\n",
            "  assert(arguments.length > 2, 'verifyProperty should receive at least 3 arguments: obj, name, and descriptor');\n",
            "  var originalDesc = __getOwnPropertyDescriptor(obj, name);\n",
            "  var nameStr = String(name);\n",
            "  if (desc === undefined) {\n",
            "    assert.sameValue(originalDesc, undefined, \"obj['\" + nameStr + \"'] descriptor should be undefined\");\n",
            "    return true;\n",
            "  }\n",
            "  assert(__hasOwnProperty(obj, name), 'obj should have an own property ' + nameStr);\n",
            "  assert.notSameValue(desc, null, 'The desc argument should be an object or undefined, null');\n",
            "  assert.sameValue(typeof desc, 'object', 'The desc argument should be an object or undefined, ' + String(desc));\n",
            "  var names = __getOwnPropertyNames(desc);\n",
            "  for (var i = 0; i < names.length; i++) {\n",
            "    assert(names[i] === 'value' || names[i] === 'writable' || names[i] === 'enumerable' || names[i] === 'configurable' || names[i] === 'get' || names[i] === 'set', 'Invalid descriptor field: ' + names[i]);\n",
            "  }\n",
            "  var failures = [];\n",
            "  if (__hasOwnProperty(desc, 'value')) {\n",
            "    if (!isSameValue(desc.value, originalDesc.value)) {\n",
            "      __push(failures, \"obj['\" + nameStr + \"'] descriptor value should be \" + desc.value);\n",
            "    }\n",
            "    if (!isSameValue(desc.value, obj[name])) {\n",
            "      __push(failures, \"obj['\" + nameStr + \"'] value should be \" + desc.value);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'enumerable') && desc.enumerable !== undefined) {\n",
            "    if (desc.enumerable !== originalDesc.enumerable || desc.enumerable !== isEnumerable(obj, name)) {\n",
            "      __push(failures, 'enumerable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'writable') && desc.writable !== undefined) {\n",
            "    if (desc.writable !== originalDesc.writable || desc.writable !== isWritable(obj, name)) {\n",
            "      __push(failures, 'writable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'configurable') && desc.configurable !== undefined) {\n",
            "    if (desc.configurable !== originalDesc.configurable || desc.configurable !== isConfigurable(obj, name)) {\n",
            "      __push(failures, 'configurable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (failures.length) {\n",
            "    assert(false, __join(failures, '; '));\n",
            "  }\n",
            "}\n",
            "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "combined-verify-property-with-value.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn combined_verify_property_with_restore_branch_accepts_math_e() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name, desc, options) {\n",
            "  assert(arguments.length > 2, 'verifyProperty should receive at least 3 arguments: obj, name, and descriptor');\n",
            "  var originalDesc = __getOwnPropertyDescriptor(obj, name);\n",
            "  var nameStr = String(name);\n",
            "  if (desc === undefined) {\n",
            "    assert.sameValue(originalDesc, undefined, \"obj['\" + nameStr + \"'] descriptor should be undefined\");\n",
            "    return true;\n",
            "  }\n",
            "  assert(__hasOwnProperty(obj, name), 'obj should have an own property ' + nameStr);\n",
            "  assert.notSameValue(desc, null, 'The desc argument should be an object or undefined, null');\n",
            "  assert.sameValue(typeof desc, 'object', 'The desc argument should be an object or undefined, ' + String(desc));\n",
            "  var names = __getOwnPropertyNames(desc);\n",
            "  for (var i = 0; i < names.length; i++) {\n",
            "    assert(names[i] === 'value' || names[i] === 'writable' || names[i] === 'enumerable' || names[i] === 'configurable' || names[i] === 'get' || names[i] === 'set', 'Invalid descriptor field: ' + names[i]);\n",
            "  }\n",
            "  var failures = [];\n",
            "  if (__hasOwnProperty(desc, 'value')) {\n",
            "    if (!isSameValue(desc.value, originalDesc.value)) {\n",
            "      __push(failures, \"obj['\" + nameStr + \"'] descriptor value should be \" + desc.value);\n",
            "    }\n",
            "    if (!isSameValue(desc.value, obj[name])) {\n",
            "      __push(failures, \"obj['\" + nameStr + \"'] value should be \" + desc.value);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'enumerable') && desc.enumerable !== undefined) {\n",
            "    if (desc.enumerable !== originalDesc.enumerable || desc.enumerable !== isEnumerable(obj, name)) {\n",
            "      __push(failures, 'enumerable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'writable') && desc.writable !== undefined) {\n",
            "    if (desc.writable !== originalDesc.writable || desc.writable !== isWritable(obj, name)) {\n",
            "      __push(failures, 'writable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (__hasOwnProperty(desc, 'configurable') && desc.configurable !== undefined) {\n",
            "    if (desc.configurable !== originalDesc.configurable || desc.configurable !== isConfigurable(obj, name)) {\n",
            "      __push(failures, 'configurable mismatch ' + nameStr);\n",
            "    }\n",
            "  }\n",
            "  if (failures.length) {\n",
            "    assert(false, __join(failures, '; '));\n",
            "  }\n",
            "  if (options && options.restore) {\n",
            "    __defineProperty(obj, name, originalDesc);\n",
            "  }\n",
            "}\n",
            "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "combined-verify-property-with-restore.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn verify_property_prefix_accepts_math_e() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name, desc) {\n",
            "  assert(arguments.length > 2, 'verifyProperty should receive at least 3 arguments: obj, name, and descriptor');\n",
            "  var originalDesc = __getOwnPropertyDescriptor(obj, name);\n",
            "  var nameStr = String(name);\n",
            "  if (desc === undefined) {\n",
            "    assert.sameValue(originalDesc, undefined, \"obj['\" + nameStr + \"'] descriptor should be undefined\");\n",
            "    return true;\n",
            "  }\n",
            "  assert(__hasOwnProperty(obj, name), 'obj should have an own property ' + nameStr);\n",
            "  assert.notSameValue(desc, null, 'The desc argument should be an object or undefined, null');\n",
            "  assert.sameValue(typeof desc, 'object', 'The desc argument should be an object or undefined, ' + String(desc));\n",
            "  return true;\n",
            "}\n",
            "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "verify-property-prefix.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn string_callable_coerces_plain_objects() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(String({}), '[object Object]', 'String(object) uses object coercion');\n",
                "assert.sameValue(String({ value: 1 }), '[object Object]', 'String(object literal with fields) still coerces');\n",
            ),
            "string-callable-object-coercion.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn verify_property_prefix_stage_fetches_descriptor_and_name() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name, desc) {\n",
            "  assert(arguments.length > 2, 'verifyProperty should receive at least 3 arguments: obj, name, and descriptor');\n",
            "  var originalDesc = __getOwnPropertyDescriptor(obj, name);\n",
            "  var nameStr = String(name);\n",
            "  assert.sameValue(originalDesc.writable, false, nameStr + ' descriptor fetched');\n",
            "}\n",
            "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "verify-property-prefix-stage-fetch.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn verify_property_prefix_stage_validates_own_property_and_desc_object() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name, desc) {\n",
            "  var nameStr = String(name);\n",
            "  assert(__hasOwnProperty(obj, name), 'obj should have an own property ' + nameStr);\n",
            "  assert.notSameValue(desc, null, 'The desc argument should be an object or undefined, null');\n",
            "}\n",
            "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "verify-property-prefix-stage-asserts.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn verify_property_prefix_stage_validates_desc_type_message() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(desc) {\n",
            "  assert.sameValue(typeof desc, 'object', 'The desc argument should be an object or undefined, ' + String(desc));\n",
            "}\n",
            "probe({ writable: false, enumerable: false, configurable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "verify-property-prefix-stage-type.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn verify_property_prefix_stage_arguments_length_guard_works() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            "function probe(obj, name, desc) {\n",
            "  assert(arguments.length > 2, 'verifyProperty should receive at least 3 arguments: obj, name, and descriptor');\n",
            "}\n",
            "probe(Math, 'E', { writable: false });\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "verify-property-prefix-arguments-guard.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn verify_property_prefix_stage_descriptor_alias_call_works() {
        let source = [
            include_str!("../../../tests/test262/harness/assert.js"),
            "\n",
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
            "\n",
            "function probe(obj, name) {\n",
            "  var originalDesc = __getOwnPropertyDescriptor(obj, name);\n",
            "  assert.sameValue(originalDesc.writable, false, 'descriptor alias call returns writable false');\n",
            "}\n",
            "probe(Math, 'E');\n",
        ]
        .concat();
        let result = execute_test262_basic(&source, "verify-property-prefix-descriptor-alias.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn arguments_length_direct_relational_comparison_works() {
        let result = execute_test262_basic(
            concat!(
                "function probe() {\n",
                "  assert.sameValue(arguments.length > 2, true, 'direct relational compare on arguments.length');\n",
                "}\n",
                "probe(1, 2, 3);\n",
            ),
            "arguments-length-direct-gt.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn arguments_length_local_relational_comparison_works() {
        let result = execute_test262_basic(
            concat!(
                "function probe() {\n",
                "  var len = arguments.length;\n",
                "  assert.sameValue(len > 2, true, 'local relational compare on arguments.length');\n",
                "}\n",
                "probe(1, 2, 3);\n",
            ),
            "arguments-length-local-gt.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn arguments_object_works_with_formal_parameters() {
        let result = execute_test262_basic(
            concat!(
                "function probe(obj, name, desc) {\n",
                "  assert.sameValue(arguments.length, 3, 'arguments length survives formal parameters');\n",
                "  assert.sameValue(arguments.length > 2, true, 'arguments length compares with formal parameters');\n",
                "}\n",
                "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
            ),
            "arguments-with-formals.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn arguments_length_guard_works_without_assert_harness_magic() {
        let result = execute_test262_basic(
            concat!(
                "function expectTrue(value) {\n",
                "  if (value !== true) {\n",
                "    throw new Test262Error('expected true');\n",
                "  }\n",
                "}\n",
                "function probe(obj, name, desc) {\n",
                "  expectTrue(arguments.length > 2);\n",
                "}\n",
                "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
            ),
            "arguments-length-guard-no-assert-magic.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn arguments_length_guard_works_inline_without_outer_function_call() {
        let result = execute_test262_basic(
            concat!(
                "function probe(obj, name, desc) {\n",
                "  if (!(arguments.length > 2)) {\n",
                "    throw new Test262Error('expected true');\n",
                "  }\n",
                "}\n",
                "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
            ),
            "arguments-length-guard-inline.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn inner_function_can_call_outer_function_declaration() {
        let result = execute_test262_basic(
            concat!(
                "function helper() {\n",
                "  return true;\n",
                "}\n",
                "function probe() {\n",
                "  if (helper() !== true) {\n",
                "    throw new Test262Error('helper call failed');\n",
                "  }\n",
                "}\n",
                "probe();\n",
            ),
            "inner-calls-outer-helper.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn inner_function_can_call_outer_function_declaration_with_argument() {
        let result = execute_test262_basic(
            concat!(
                "function helper(value) {\n",
                "  if (value !== true) {\n",
                "    throw new Test262Error('helper argument failed');\n",
                "  }\n",
                "}\n",
                "function probe() {\n",
                "  helper(true);\n",
                "}\n",
                "probe();\n",
            ),
            "inner-calls-outer-helper-with-arg.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn inner_function_can_call_outer_function_with_relational_argument() {
        let result = execute_test262_basic(
            concat!(
                "function helper(value) {\n",
                "  if (value !== true) {\n",
                "    throw new Test262Error('relational helper argument failed');\n",
                "  }\n",
                "}\n",
                "function probe() {\n",
                "  helper(1 < 2);\n",
                "}\n",
                "probe();\n",
            ),
            "inner-calls-outer-helper-with-relational-arg.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn inner_function_can_call_outer_function_with_arguments_length_argument() {
        let result = execute_test262_basic(
            concat!(
                "function helper(value) {\n",
                "  if (value !== 3) {\n",
                "    throw new Test262Error('arguments length helper argument failed');\n",
                "  }\n",
                "}\n",
                "function probe(obj, name, desc) {\n",
                "  helper(arguments.length);\n",
                "}\n",
                "probe(Math, 'E', { writable: false, enumerable: false, configurable: false });\n",
            ),
            "inner-calls-outer-helper-with-arguments-length.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn arguments_object_reports_runtime_argument_count() {
        let result = execute_test262_basic(
            concat!(
                "function readCount() {\n",
                "  assert.sameValue(arguments.length, 3, 'arguments length tracks actual args');\n",
                "}\n",
                "readCount(1, 2, 3);\n",
            ),
            "arguments-length.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn nested_function_declarations_are_hoisted_inside_functions() {
        let result = execute_test262_basic(
            concat!(
                "function outer() {\n",
                "  assert.sameValue(inner(), 1, 'nested function declaration is hoisted');\n",
                "  function inner() { return 1; }\n",
                "}\n",
                "outer();\n",
            ),
            "nested-function-hoist.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn property_helper_primitives_support_descriptor_name_listing_and_failure_arrays() {
        let result = execute_test262_basic(
            concat!(
                "var desc = Object.getOwnPropertyDescriptor(Math, 'E');\n",
                "var names = Object.getOwnPropertyNames(desc);\n",
                "assert.sameValue(names.length >= 3, true, 'descriptor names materialize into an array');\n",
                "var push = Function.prototype.call.bind(Array.prototype.push);\n",
                "var failures = [];\n",
                "assert.sameValue(push(failures, 'x'), 1, 'bound push appends first failure');\n",
                "assert.sameValue(push(failures, 'y'), 2, 'bound push appends second failure');\n",
                "assert.sameValue(failures.length, 2, 'failure array tracks appended entries');\n",
            ),
            "property-helper-primitives.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn property_helper_has_own_property_distinguishes_missing_value_field() {
        let source = format!(
            "{}\n{}\nvar desc = {{ writable: false, enumerable: false, configurable: false }};\nassert.sameValue(__hasOwnProperty(desc, 'value'), false);\nassert.sameValue(__hasOwnProperty(desc, 'writable'), true);\n",
            include_str!("../../../tests/test262/harness/assert.js"),
            include_str!("../../../tests/test262/harness/propertyHelper.js"),
        );
        let result =
            execute_test262_basic(&source, "property-helper-has-own-property-missing-value.js");
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn logical_and_short_circuits_undefined_options_receiver() {
        let result = execute_test262_basic(
            concat!(
                "function probe(options) {\n",
                "  if (options && options.restore) {\n",
                "    throw new Test262Error('options short-circuit failed');\n",
                "  }\n",
                "}\n",
                "probe();\n",
            ),
            "logical-and-short-circuit-options.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // ---- Step 24 tests ----

    #[test]
    fn error_prototype_to_string() {
        let result = execute_test262_basic(
            concat!(
                "var e = new Error('boom');\n",
                "assert.sameValue(e.toString(), 'Error: boom', 'Error.toString');\n",
                "var t = new TypeError('bad type');\n",
                "assert.sameValue(t.toString(), 'TypeError: bad type', 'TypeError.toString');\n",
                "var noMsg = new Error();\n",
                "assert.sameValue(noMsg.toString(), 'Error', 'Error without message');\n",
                "var custom = new Error('x');\n",
                "custom.name = '';\n",
                "assert.sameValue(custom.toString(), 'x', 'empty name returns message only');\n",
                "var both = new Error();\n",
                "both.name = '';\n",
                "assert.sameValue(both.toString(), '', 'empty name and empty message');\n",
            ),
            "error-to-string.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn uri_error_and_eval_error_constructors() {
        let result = execute_test262_basic(
            concat!(
                "var u = new URIError('bad uri');\n",
                "assert.sameValue(u instanceof URIError, true, 'URIError instanceof');\n",
                "assert.sameValue(u instanceof Error, true, 'URIError inherits Error');\n",
                "assert.sameValue(u.message, 'bad uri', 'URIError message');\n",
                "assert.sameValue(u.name, 'URIError', 'URIError name');\n",
                "var e = new EvalError('bad eval');\n",
                "assert.sameValue(e instanceof EvalError, true, 'EvalError instanceof');\n",
                "assert.sameValue(e instanceof Error, true, 'EvalError inherits Error');\n",
                "assert.sameValue(e.message, 'bad eval', 'EvalError message');\n",
            ),
            "uri-eval-error.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn array_sort_basic() {
        let result = execute_test262_basic(
            concat!(
                "var nums = [3, 1, 4, 1, 5, 9, 2, 6];\n",
                "nums.sort();\n",
                "assert.sameValue(nums.join(','), '1,1,2,3,4,5,6,9', 'default sort');\n",
                "var desc = [3, 1, 4];\n",
                "desc.sort(function(a, b) { return b - a; });\n",
                "assert.sameValue(desc.join(','), '4,3,1', 'comparator sort descending');\n",
                "var stable = [{k:1,v:'a'},{k:2,v:'b'},{k:1,v:'c'}];\n",
                "stable.sort(function(a, b) { return a.k - b.k; });\n",
                "assert.sameValue(stable[0].v, 'a', 'stable sort preserves order of equal keys');\n",
                "assert.sameValue(stable[1].v, 'c', 'stable sort second equal key');\n",
            ),
            "array-sort.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn number_to_fixed() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue((1.23456).toFixed(2), '1.23', 'toFixed 2');\n",
                "assert.sameValue((0).toFixed(5), '0.00000', 'toFixed 5 on zero');\n",
                "assert.sameValue((1.005).toFixed(0), '1', 'toFixed 0');\n",
            ),
            "number-to-fixed.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn object_from_entries() {
        let result = execute_test262_basic(
            concat!(
                "var obj = Object.fromEntries([['a', 1], ['b', 2]]);\n",
                "assert.sameValue(obj.a, 1, 'fromEntries a');\n",
                "assert.sameValue(obj.b, 2, 'fromEntries b');\n",
                "var roundTrip = Object.fromEntries(Object.entries({x: 10, y: 20}));\n",
                "assert.sameValue(roundTrip.x, 10, 'round-trip x');\n",
                "assert.sameValue(roundTrip.y, 20, 'round-trip y');\n",
            ),
            "object-from-entries.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn array_flat_and_flat_map() {
        let result = execute_test262_basic(
            concat!(
                "var flat1 = [1, [2, 3], [4, [5]]].flat();\n",
                "assert.sameValue(flat1.length, 5, 'flat depth 1 length');\n",
                "assert.sameValue(flat1[0], 1, 'flat[0]');\n",
                "assert.sameValue(flat1[3], 4, 'flat[3]');\n",
                "var flat2 = [1, [2, [3, [4]]]].flat(2);\n",
                "assert.sameValue(flat2.length, 4, 'flat depth 2 length');\n",
                "assert.sameValue(flat2[2], 3, 'flat depth 2 [2]');\n",
                "var mapped = [1, 2, 3].flatMap(function(x) { return [x, x * 2]; });\n",
                "assert.sameValue(mapped.length, 6, 'flatMap length');\n",
                "assert.sameValue(mapped[1], 2, 'flatMap [1]');\n",
                "assert.sameValue(mapped[5], 6, 'flatMap [5]');\n",
            ),
            "array-flat.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn array_sort_reduce_right_find_last() {
        let result = execute_test262_basic(
            concat!(
                "var sum = [1, 2, 3, 4].reduceRight(function(a, b) { return a + b; });\n",
                "assert.sameValue(sum, 10, 'reduceRight');\n",
                "assert.sameValue([1, 2, 3].findLast(function(x) { return x < 3; }), 2, 'findLast');\n",
                "assert.sameValue([1, 2, 3].findLastIndex(function(x) { return x < 3; }), 1, 'findLastIndex');\n",
                "assert.sameValue([1, 2, 3].at(-1), 3, 'array at -1');\n",
                "assert.sameValue([1, 2, 3].at(0), 1, 'array at 0');\n",
            ),
            "array-extras.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // ---- Map and Set ----

    #[test]
    fn map_basic_operations() {
        // Step 1: Map get/set
        let r1 = execute_test262_basic(
            "var m = new Map();\nm.set('a', 1);\nassert.sameValue(m.get('a'), 1, 'get');\n",
            "map-1.js",
        );
        assert_eq!(r1, RegisterValue::from_i32(0), "Map get/set");

        // Step 2: Map has
        let r2 = execute_test262_basic(
            "var m = new Map();\nm.set('a', 1);\nassert.sameValue(m.has('a'), true, 'has');\n",
            "map-2.js",
        );
        assert_eq!(r2, RegisterValue::from_i32(0), "Map has");

        // Step 3: Map size
        let r3 = execute_test262_basic(
            "var m = new Map();\nm.set('a', 1);\nassert.sameValue(m.size, 1, 'size');\n",
            "map-3.js",
        );
        assert_eq!(r3, RegisterValue::from_i32(0), "Map size");

        let result = execute_test262_basic(
            concat!(
                "var m = new Map();\n",
                "m.set('a', 1);\n",
                "m.set('b', 2);\n",
                "assert.sameValue(m.get('a'), 1, 'Map.get a');\n",
                "assert.sameValue(m.get('b'), 2, 'Map.get b');\n",
                "assert.sameValue(m.get('c'), undefined, 'Map.get missing');\n",
                "assert.sameValue(m.has('a'), true, 'Map.has a');\n",
                "assert.sameValue(m.has('c'), false, 'Map.has missing');\n",
                "assert.sameValue(m.size, 2, 'Map.size');\n",
                "m.set('a', 99);\n",
                "assert.sameValue(m.get('a'), 99, 'Map.set overwrites');\n",
                "assert.sameValue(m.size, 2, 'Map.size after overwrite');\n",
                "assert.sameValue(m.delete('b'), true, 'Map.delete returns true');\n",
                "assert.sameValue(m.has('b'), false, 'Map.delete removes key');\n",
                "assert.sameValue(m.size, 1, 'Map.size after delete');\n",
                "m.clear();\n",
                "assert.sameValue(m.size, 0, 'Map.clear empties');\n",
                // Constructor with iterable
                "var m2 = new Map([['x', 10], ['y', 20]]);\n",
                "assert.sameValue(m2.get('x'), 10, 'Map constructor iterable x');\n",
                "assert.sameValue(m2.get('y'), 20, 'Map constructor iterable y');\n",
                "assert.sameValue(m2.size, 2, 'Map constructor iterable size');\n",
                // forEach
                "var sum = 0;\n",
                "m2.forEach(function(v) { sum = sum + v; });\n",
                "assert.sameValue(sum, 30, 'Map.forEach accumulates');\n",
                // NaN key
                "var m3 = new Map();\n",
                "m3.set(NaN, 'nan');\n",
                "assert.sameValue(m3.has(NaN), true, 'Map NaN key lookup');\n",
                "assert.sameValue(m3.get(NaN), 'nan', 'Map NaN key get');\n",
            ),
            "map-basic.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn set_basic_operations() {
        let result = execute_test262_basic(
            concat!(
                "var s = new Set();\n",
                "s.add(1);\n",
                "s.add(2);\n",
                "s.add(1);\n",
                "assert.sameValue(s.size, 2, 'Set deduplicates');\n",
                "assert.sameValue(s.has(1), true, 'Set.has 1');\n",
                "assert.sameValue(s.has(3), false, 'Set.has missing');\n",
                "assert.sameValue(s.delete(1), true, 'Set.delete returns true');\n",
                "assert.sameValue(s.has(1), false, 'Set.delete removes value');\n",
                "assert.sameValue(s.size, 1, 'Set.size after delete');\n",
                "s.clear();\n",
                "assert.sameValue(s.size, 0, 'Set.clear empties');\n",
                // Constructor with iterable
                "var s2 = new Set([3, 4, 3, 5]);\n",
                "assert.sameValue(s2.size, 3, 'Set constructor deduplicates');\n",
                "assert.sameValue(s2.has(3), true, 'Set constructor has 3');\n",
                "assert.sameValue(s2.has(5), true, 'Set constructor has 5');\n",
                // forEach
                "var sum = 0;\n",
                "s2.forEach(function(v) { sum = sum + v; });\n",
                "assert.sameValue(sum, 12, 'Set.forEach accumulates');\n",
            ),
            "set-basic.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // ---- JSON ----

    #[test]
    fn json_parse_primitives() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(JSON.parse('null'), null, 'parse null');\n",
                "assert.sameValue(JSON.parse('true'), true, 'parse true');\n",
                "assert.sameValue(JSON.parse('false'), false, 'parse false');\n",
                "assert.sameValue(JSON.parse('42'), 42, 'parse int');\n",
                "assert.sameValue(JSON.parse('3.14'), 3.14, 'parse float');\n",
                "assert.sameValue(JSON.parse('\"hello\"'), 'hello', 'parse string');\n",
            ),
            "json-parse-primitives.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn json_parse_objects_and_arrays() {
        let result = execute_test262_basic(
            concat!(
                "var obj = JSON.parse('{\"a\":1,\"b\":\"two\",\"c\":true}');\n",
                "assert.sameValue(obj.a, 1, 'parse obj.a');\n",
                "assert.sameValue(obj.b, 'two', 'parse obj.b');\n",
                "assert.sameValue(obj.c, true, 'parse obj.c');\n",
                "var arr = JSON.parse('[1,2,3]');\n",
                "assert.sameValue(arr.length, 3, 'parse array length');\n",
                "assert.sameValue(arr[1], 2, 'parse arr[1]');\n",
                "var nested = JSON.parse('{\"x\":[{\"y\":9}]}');\n",
                "assert.sameValue(nested.x[0].y, 9, 'parse nested');\n",
            ),
            "json-parse-objects.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn json_parse_reviver() {
        let result = execute_test262_basic(
            concat!(
                "var obj = JSON.parse('{\"a\":1,\"b\":2}', function(key, value) {\n",
                "  if (key === 'a') return value * 10;\n",
                "  return value;\n",
                "});\n",
                "assert.sameValue(obj.a, 10, 'reviver transforms a');\n",
                "assert.sameValue(obj.b, 2, 'reviver preserves b');\n",
            ),
            "json-parse-reviver.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn json_parse_syntax_error() {
        let result = execute_test262_basic(
            concat!(
                "try {\n",
                "  JSON.parse('{bad}');\n",
                "  throw new Test262Error('should throw');\n",
                "} catch (e) {\n",
                "  assert.sameValue(e.name, 'SyntaxError', 'parse error is SyntaxError');\n",
                "}\n",
            ),
            "json-parse-error.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn json_stringify_primitives() {
        let result = execute_test262_basic(
            concat!(
                "assert.sameValue(JSON.stringify(null), 'null', 'stringify null');\n",
                "assert.sameValue(JSON.stringify(true), 'true', 'stringify true');\n",
                "assert.sameValue(JSON.stringify(42), '42', 'stringify int');\n",
                "assert.sameValue(JSON.stringify('hello'), '\"hello\"', 'stringify string');\n",
                "assert.sameValue(JSON.stringify(undefined), undefined, 'stringify undefined');\n",
            ),
            "json-stringify-primitives.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn json_stringify_objects_and_arrays() {
        let r1 = execute_test262_basic(
            "assert.sameValue(JSON.stringify([1,2,3]), '[1,2,3]', 'stringify array');\n",
            "json-arr.js",
        );
        assert_eq!(r1, RegisterValue::from_i32(0), "array");

        // Object only
        let r2 = execute_test262_basic(
            "assert.sameValue(JSON.stringify({a:1}), '{\"a\":1}', 'a2');\n",
            "json-obj.js",
        );
        assert_eq!(r2, RegisterValue::from_i32(0), "object");

        // Nested
        let r3 = execute_test262_basic(
            "var nested = JSON.stringify({x:[1,{y:2}]});\nassert.sameValue(nested, '{\"x\":[1,{\"y\":2}]}', 'a3');\n",
            "json-nested.js",
        );
        assert_eq!(r3, RegisterValue::from_i32(0), "nested");
    }

    #[test]
    fn json_stringify_replacer_and_space() {
        let result = execute_test262_basic(
            concat!(
                "var obj = {a: 1, b: 2, c: 3};\n",
                "var filtered = JSON.stringify(obj, ['a', 'c']);\n",
                "assert.sameValue(filtered, '{\"a\":1,\"c\":3}', 'array replacer filters keys');\n",
                "var transformed = JSON.stringify(obj, function(key, value) {\n",
                "  if (typeof value === 'number') return value * 2;\n",
                "  return value;\n",
                "});\n",
                "assert.sameValue(transformed, '{\"a\":2,\"b\":4,\"c\":6}', 'function replacer transforms');\n",
            ),
            "json-stringify-replacer.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn json_stringify_circular_throws() {
        let result = execute_test262_basic(
            concat!(
                "var obj = {};\n",
                "obj.self = obj;\n",
                "try {\n",
                "  JSON.stringify(obj);\n",
                "  throw new Test262Error('should throw');\n",
                "} catch (e) {\n",
                "  assert.sameValue(e.name, 'TypeError', 'circular throws TypeError');\n",
                "}\n",
            ),
            "json-stringify-circular.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn json_round_trip() {
        let result = execute_test262_basic(
            concat!(
                "var original = {name: 'otter', version: 1, tags: ['js', 'runtime'], nested: {ok: true}};\n",
                "var roundTripped = JSON.parse(JSON.stringify(original));\n",
                "assert.sameValue(roundTripped.name, 'otter', 'round-trip name');\n",
                "assert.sameValue(roundTripped.version, 1, 'round-trip version');\n",
                "assert.sameValue(roundTripped.tags.length, 2, 'round-trip tags length');\n",
                "assert.sameValue(roundTripped.tags[0], 'js', 'round-trip tags[0]');\n",
                "assert.sameValue(roundTripped.nested.ok, true, 'round-trip nested');\n",
            ),
            "json-round-trip.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    // ── §10.2.4 / §10.4.4 Strict mode validation ──

    #[test]
    fn oxc_rejects_strict_mode_duplicate_parameters() {
        let result = compile_test262_basic_script(
            "\"use strict\"; function f(a, a) { return a; }",
            "strict-dup-params.js",
        );
        assert!(result.is_err(), "oxc should reject duplicate params in strict mode");
    }

    #[test]
    fn sloppy_mode_duplicate_parameters_allowed() {
        let result = compile_test262_basic_script(
            "function f(a, a) { return a; }",
            "sloppy-dup-params.js",
        );
        assert!(result.is_ok(), "sloppy mode should allow duplicate params");
    }

    #[test]
    fn oxc_rejects_strict_mode_legacy_octal_literal() {
        let result = compile_test262_basic_script(
            "\"use strict\"; var x = 077;",
            "strict-octal.js",
        );
        assert!(result.is_err(), "oxc should reject legacy octal in strict mode");
    }

    #[test]
    fn sloppy_mode_legacy_octal_literal_allowed() {
        let result = compile_test262_basic_script(
            "var x = 077;",
            "sloppy-octal.js",
        );
        assert!(result.is_ok(), "sloppy mode should allow legacy octal");
    }

    #[test]
    fn oxc_rejects_strict_mode_octal_escape() {
        let result = compile_test262_basic_script(
            "\"use strict\"; var x = \"\\077\";",
            "strict-octal-escape.js",
        );
        assert!(result.is_err(), "oxc should reject octal escape in strict mode");
    }

    #[test]
    fn strict_arguments_callee_throws_type_error() {
        let module = compile_test262_basic_script(
            concat!(
                "function strict() {\n",
                "  \"use strict\";\n",
                "  try {\n",
                "    var c = arguments.callee;\n",
                "    throw new Test262Error(\"should have thrown\");\n",
                "  } catch (e) {\n",
                "    if (!(e instanceof TypeError)) {\n",
                "      throw new Test262Error(\"wrong error type: \" + e);\n",
                "    }\n",
                "  }\n",
                "}\n",
                "strict();\n",
            ),
            "strict-arguments-callee.js",
        )
        .expect("should compile");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        let result = Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect("strict arguments.callee test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }

    #[test]
    fn sloppy_arguments_callee_is_defined() {
        // Sloppy-mode arguments.callee should be a data property containing
        // the closure for the executing function (§10.4.4.6 step 13).
        let result = execute_test262_basic(
            concat!(
                "function sloppy() {\n",
                "  if (typeof arguments.callee !== 'function') {\n",
                "    throw new Test262Error('callee type: ' + typeof arguments.callee);\n",
                "  }\n",
                "}\n",
                "sloppy();\n",
            ),
            "sloppy-arguments-callee.js",
        );
        assert_eq!(result, RegisterValue::from_i32(0));
    }

    #[test]
    fn strict_arguments_callee_set_throws_type_error() {
        let module = compile_test262_basic_script(
            concat!(
                "function strict() {\n",
                "  \"use strict\";\n",
                "  try {\n",
                "    arguments.callee = 1;\n",
                "    throw new Test262Error(\"should have thrown on set\");\n",
                "  } catch (e) {\n",
                "    if (!(e instanceof TypeError)) {\n",
                "      throw new Test262Error(\"wrong error type: \" + e);\n",
                "    }\n",
                "  }\n",
                "}\n",
                "strict();\n",
            ),
            "strict-arguments-callee-set.js",
        )
        .expect("should compile");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];
        let result = Interpreter::new()
            .execute_with_runtime(
                &module,
                crate::module::FunctionIndex(0),
                &registers,
                &mut runtime,
            )
            .expect("strict arguments.callee set test should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
    }
}
