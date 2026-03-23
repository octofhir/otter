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
    let program = lower_script(source, source_url)?;
    lowering::compile_module(&program).map_err(Into::into)
}

/// Parse, lower, and compile a tiny native Test262 script into an `otter-vm` module.
pub fn compile_test262_basic_script(
    source: &str,
    source_url: &str,
) -> Result<Module, SourceLoweringError> {
    let program = lower_script_with_mode(source, source_url, LoweringMode::Test262Basic)?;
    lowering::compile_module(&program).map_err(Into::into)
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
    let mut source_type = SourceType::from_path(source_url)
        .unwrap_or_else(|_| SourceType::default().with_script(true))
        .with_script(true);

    if source_type.is_typescript() || source_type.is_jsx() {
        return Err(SourceLoweringError::Unsupported(
            "TypeScript and JSX source are not enabled on the tiny new-VM path".to_string(),
        ));
    }

    source_type = source_type.with_module(false);

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if let Some(error) = parsed.errors.first() {
        return Err(SourceLoweringError::Parse(error.to_string()));
    }

    TinyScriptLowerer::new(source_url, mode).lower_program(&parsed.program)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoweringMode {
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

            let local = self.declare_binding(identifier.name.as_str())?;
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
                let statement = self.lower_assignment_expression(assignment)?;
                output.push(statement);
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
    ) -> Result<Statement, SourceLoweringError> {
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
        Ok(Statement::assign(local, value))
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

                Ok(Expr::local(self.lookup_binding(identifier.name.as_str())?))
            }
            Expression::ParenthesizedExpression(parenthesized) => {
                self.lower_expression(&parenthesized.expression)
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
}
