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
    use crate::source::{compile_script, compile_test262_basic_script, lower_script};
    use crate::value::RegisterValue;

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
                "assert.sameValue(Reflect.set(child, \"value\", 9), true, \"Reflect.set reports success\");\n",
                "assert.sameValue(child.value, 9, \"Reflect.set writes onto receiver\");\n",
                "assert.sameValue(proto.value, 7, \"Reflect.set keeps prototype slot intact\");\n",
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
}
