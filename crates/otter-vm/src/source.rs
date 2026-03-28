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
            LoweringMode::Script => {
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
    use crate::Interpreter;
    use crate::interpreter::InterpreterError;
    use crate::source::{compile_script, compile_test262_basic_script, lower_script};
    use crate::value::RegisterValue;

    fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
        let module = compile_test262_basic_script(source, source_url)
            .expect("test262 basic script compiles");

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
            .expect("test262 basic script executes")
            .return_value()
    }

    fn execute_test262_basic_error(source: &str, source_url: &str) -> InterpreterError {
        let module = compile_test262_basic_script(source, source_url)
            .expect("test262 basic script compiles");

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
            ),
            "native-test262-closures-objects.js",
        )
        .expect("closure/object script should compile");

        let result = Interpreter::new()
            .execute(&module)
            .expect("closure/object script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
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
                "assert.sameValue(box.value, 7, \"primitive return falls back to receiver\");\n",
                "assert.sameValue(override.value, 9, \"object return overrides receiver\");\n",
                "assert.sameValue(box.constructor, Box, \"closure prototype constructor link\");\n",
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
            ),
            "native-test262-array-reflect.js",
        )
        .expect("array/reflect script should compile");

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
            .expect("array/reflect script should execute");
        assert_eq!(result.return_value(), RegisterValue::from_i32(0));
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
                matches!(error, InterpreterError::TypeError(_)),
                "expected TypeError for {source_url}, got {error:?}"
            );
            assert!(
                error.to_string().contains("null or undefined"),
                "unexpected error for {source_url}: {error}"
            );
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
                "} catch (error) {}\n",
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
                "try {\n",
                "  Object.defineProperty({}, 'broken', undefined);\n",
                "  throw new Test262Error('defineProperty should reject non-object descriptor');\n",
                "} catch (error) {}\n",
                "try {\n",
                "  Object.defineProperties({}, { broken: 1 });\n",
                "  throw new Test262Error('defineProperties should reject non-object descriptor entry');\n",
                "} catch (error) {}\n",
            ),
            "native-test262-object-define-properties.js",
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
                "try {\n",
                "  Object.setPrototypeOf(object, null);\n",
                "  throw new Test262Error('Object.setPrototypeOf should throw on non-extensible target');\n",
                "} catch (error) {}\n",
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
            error.to_string().contains("Cannot use 'in' operator"),
            "unexpected error: {error}"
        );
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
            error.to_string().contains("Cannot use 'in' operator"),
            "unexpected error: {error}"
        );
    }
}
