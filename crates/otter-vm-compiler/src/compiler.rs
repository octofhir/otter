//! Main compiler implementation

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

use otter_vm_bytecode::{Instruction, JumpOffset, LocalIndex, Register};

use crate::codegen::CodeGen;
use crate::error::{CompileError, CompileResult};
use crate::scope::ResolvedBinding;

/// The compiler
pub struct Compiler {
    /// Code generator
    codegen: CodeGen,
}

impl Compiler {
    /// Create a new compiler
    pub fn new() -> Self {
        Self {
            codegen: CodeGen::new(),
        }
    }

    /// Compile source code to a module
    pub fn compile(
        mut self,
        source: &str,
        source_url: &str,
    ) -> CompileResult<otter_vm_bytecode::Module> {
        // Parse with oxc
        let allocator = Allocator::default();
        let source_type = SourceType::from_path(source_url).unwrap_or_default();
        let parser = Parser::new(&allocator, source, source_type);
        let result = parser.parse();

        // Check for parse errors
        if !result.errors.is_empty() {
            let error = &result.errors[0];
            return Err(CompileError::Parse(error.to_string()));
        }

        // Compile the program
        let program = result.program;
        self.compile_program(&program)?;

        // Ensure we return something
        self.codegen.emit(Instruction::ReturnUndefined);

        Ok(self.codegen.finish(source_url))
    }

    /// Compile a program
    fn compile_program(&mut self, program: &Program) -> CompileResult<()> {
        for stmt in &program.body {
            self.compile_statement(stmt)?;
        }
        Ok(())
    }

    /// Compile a statement
    fn compile_statement(&mut self, stmt: &Statement) -> CompileResult<()> {
        match stmt {
            Statement::ExpressionStatement(expr_stmt) => {
                // Compile expression and discard result
                let reg = self.compile_expression(&expr_stmt.expression)?;
                self.codegen.free_reg(reg);
                Ok(())
            }

            Statement::VariableDeclaration(decl) => self.compile_variable_declaration(decl),

            Statement::ReturnStatement(ret) => {
                if let Some(arg) = &ret.argument {
                    let reg = self.compile_expression(arg)?;
                    self.codegen.emit(Instruction::Return { src: reg });
                    self.codegen.free_reg(reg);
                } else {
                    self.codegen.emit(Instruction::ReturnUndefined);
                }
                Ok(())
            }

            Statement::BlockStatement(block) => {
                self.codegen.enter_scope();
                for stmt in &block.body {
                    self.compile_statement(stmt)?;
                }
                self.codegen.exit_scope();
                Ok(())
            }

            Statement::IfStatement(if_stmt) => self.compile_if_statement(if_stmt),

            Statement::WhileStatement(while_stmt) => self.compile_while_statement(while_stmt),

            Statement::ForStatement(for_stmt) => self.compile_for_statement(for_stmt),

            Statement::FunctionDeclaration(func) => self.compile_function_declaration(func),

            Statement::EmptyStatement(_) => Ok(()),

            Statement::DebuggerStatement(_) => {
                self.codegen.emit(Instruction::Debugger);
                Ok(())
            }

            Statement::ThrowStatement(throw_stmt) => {
                let src = self.compile_expression(&throw_stmt.argument)?;
                self.codegen.emit(Instruction::Throw { src });
                self.codegen.free_reg(src);
                Ok(())
            }

            _ => Err(CompileError::unsupported("Unknown statement type")),
        }
    }

    /// Compile a variable declaration
    fn compile_variable_declaration(&mut self, decl: &VariableDeclaration) -> CompileResult<()> {
        let is_const = decl.kind == VariableDeclarationKind::Const;

        for declarator in &decl.declarations {
            match &declarator.id {
                BindingPattern::BindingIdentifier(ident) => {
                    let local_idx = self.codegen.declare_variable(&ident.name, is_const)?;

                    if let Some(init) = &declarator.init {
                        let reg = self.compile_expression(init)?;
                        self.codegen.emit(Instruction::SetLocal {
                            idx: LocalIndex(local_idx),
                            src: reg,
                        });
                        self.codegen.free_reg(reg);
                    }
                }
                _ => return Err(CompileError::unsupported("Destructuring patterns")),
            }
        }

        Ok(())
    }

    /// Compile an if statement
    fn compile_if_statement(&mut self, if_stmt: &IfStatement) -> CompileResult<()> {
        // Compile condition
        let cond = self.compile_expression(&if_stmt.test)?;
        let jump_else = self.codegen.emit_jump_if_false(cond);
        self.codegen.free_reg(cond);

        // Compile consequent
        self.compile_statement(&if_stmt.consequent)?;

        if let Some(alternate) = &if_stmt.alternate {
            // Jump over else branch
            let jump_end = self.codegen.emit_jump();

            // Patch jump to else
            let else_offset = self.codegen.current_index() as i32 - jump_else as i32;
            self.codegen.patch_jump(jump_else, else_offset);

            // Compile alternate
            self.compile_statement(alternate)?;

            // Patch jump to end
            let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
            self.codegen.patch_jump(jump_end, end_offset);
        } else {
            // Patch jump to end
            let end_offset = self.codegen.current_index() as i32 - jump_else as i32;
            self.codegen.patch_jump(jump_else, end_offset);
        }

        Ok(())
    }

    /// Compile a while statement
    fn compile_while_statement(&mut self, while_stmt: &WhileStatement) -> CompileResult<()> {
        let loop_start = self.codegen.current_index();

        // Compile condition
        let cond = self.compile_expression(&while_stmt.test)?;
        let jump_end = self.codegen.emit_jump_if_false(cond);
        self.codegen.free_reg(cond);

        // Compile body
        self.compile_statement(&while_stmt.body)?;

        // Jump back to start
        let back_offset = loop_start as i32 - self.codegen.current_index() as i32;
        self.codegen.emit(Instruction::Jump {
            offset: JumpOffset(back_offset),
        });

        // Patch jump to end
        let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
        self.codegen.patch_jump(jump_end, end_offset);

        Ok(())
    }

    /// Compile a for statement
    fn compile_for_statement(&mut self, for_stmt: &ForStatement) -> CompileResult<()> {
        self.codegen.enter_scope();

        // Compile init
        if let Some(init) = &for_stmt.init {
            match init {
                ForStatementInit::VariableDeclaration(decl) => {
                    self.compile_variable_declaration(decl)?;
                }
                _ => {
                    // Handle expression init
                    if let Some(expr) = init.as_expression() {
                        let reg = self.compile_expression(expr)?;
                        self.codegen.free_reg(reg);
                    }
                }
            }
        }

        let loop_start = self.codegen.current_index();

        // Compile test
        let jump_end = if let Some(test) = &for_stmt.test {
            let cond = self.compile_expression(test)?;
            let jump = self.codegen.emit_jump_if_false(cond);
            self.codegen.free_reg(cond);
            Some(jump)
        } else {
            None
        };

        // Compile body
        self.compile_statement(&for_stmt.body)?;

        // Compile update
        if let Some(update) = &for_stmt.update {
            let reg = self.compile_expression(update)?;
            self.codegen.free_reg(reg);
        }

        // Jump back to start
        let back_offset = loop_start as i32 - self.codegen.current_index() as i32;
        self.codegen.emit(Instruction::Jump {
            offset: JumpOffset(back_offset),
        });

        // Patch jump to end
        if let Some(jump_end) = jump_end {
            let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
            self.codegen.patch_jump(jump_end, end_offset);
        }

        self.codegen.exit_scope();
        Ok(())
    }

    /// Compile a function declaration
    fn compile_function_declaration(&mut self, func: &oxc_ast::ast::Function) -> CompileResult<()> {
        let name = func.id.as_ref().map(|id| id.name.to_string());

        // Declare function in current scope
        if let Some(ref n) = name {
            self.codegen.declare_variable(n, false)?;
        }

        // Enter function context
        self.codegen.enter_function(name.clone());

        // Declare parameters
        for param in &func.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                }
                _ => return Err(CompileError::unsupported("Complex parameter patterns")),
            }
        }

        // Compile function body
        if let Some(body) = &func.body {
            for stmt in &body.statements {
                self.compile_statement(stmt)?;
            }
        }

        // Ensure return
        self.codegen.emit(Instruction::ReturnUndefined);

        // Exit function and get index
        let func_idx = self.codegen.exit_function();

        // Create closure and store in variable
        if let Some(n) = name
            && let Some(ResolvedBinding::Local(idx)) = self.codegen.resolve_variable(&n)
        {
            let dst = self.codegen.alloc_reg();
            self.codegen.emit(Instruction::Closure {
                dst,
                func: otter_vm_bytecode::FunctionIndex(func_idx),
            });
            self.codegen.emit(Instruction::SetLocal {
                idx: LocalIndex(idx),
                src: dst,
            });
            self.codegen.free_reg(dst);
        }

        Ok(())
    }

    /// Compile a function expression
    fn compile_function_expression(
        &mut self,
        func: &oxc_ast::ast::Function,
    ) -> CompileResult<Register> {
        let name = func.id.as_ref().map(|id| id.name.to_string());

        // Enter function context
        self.codegen.enter_function(name);

        // Declare parameters
        for param in &func.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                }
                _ => return Err(CompileError::unsupported("Complex parameter patterns")),
            }
        }

        // Compile function body
        if let Some(body) = &func.body {
            for stmt in &body.statements {
                self.compile_statement(stmt)?;
            }
        }

        // Ensure return
        self.codegen.emit(Instruction::ReturnUndefined);

        // Exit function and get index
        let func_idx = self.codegen.exit_function();

        // Create closure
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Closure {
            dst,
            func: otter_vm_bytecode::FunctionIndex(func_idx),
        });

        Ok(dst)
    }

    /// Compile an arrow function expression
    fn compile_arrow_function(
        &mut self,
        arrow: &ArrowFunctionExpression,
    ) -> CompileResult<Register> {
        // Enter function context
        self.codegen.enter_function(None);
        self.codegen.current.flags.is_arrow = true;

        // Declare parameters
        for param in &arrow.params.items {
            match &param.pattern {
                BindingPattern::BindingIdentifier(ident) => {
                    self.codegen.declare_variable(&ident.name, false)?;
                    self.codegen.current.param_count += 1;
                }
                _ => return Err(CompileError::unsupported("Complex parameter patterns")),
            }
        }

        // Compile body
        if arrow.expression {
            // Expression body: `(x) => x + 1`
            // In oxc, expression body is stored as a single ExpressionStatement
            if let Some(Statement::ExpressionStatement(expr_stmt)) = arrow.body.statements.first() {
                let result = self.compile_expression(&expr_stmt.expression)?;
                self.codegen.emit(Instruction::Return { src: result });
                self.codegen.free_reg(result);
            } else {
                self.codegen.emit(Instruction::ReturnUndefined);
            }
        } else {
            // Statement body: `(x) => { return x + 1; }`
            for stmt in &arrow.body.statements {
                self.compile_statement(stmt)?;
            }
            self.codegen.emit(Instruction::ReturnUndefined);
        }

        // Exit function and get index
        let func_idx = self.codegen.exit_function();

        // Create closure
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Closure {
            dst,
            func: otter_vm_bytecode::FunctionIndex(func_idx),
        });

        Ok(dst)
    }

    /// Compile an expression
    fn compile_expression(&mut self, expr: &Expression) -> CompileResult<Register> {
        match expr {
            Expression::NumericLiteral(lit) => {
                let dst = self.codegen.alloc_reg();
                let value = lit.value;

                // Use LoadInt32 for integers that fit
                if value.fract() == 0.0 && value >= i32::MIN as f64 && value <= i32::MAX as f64 {
                    self.codegen.emit(Instruction::LoadInt32 {
                        dst,
                        value: value as i32,
                    });
                } else {
                    let idx = self.codegen.add_number(value);
                    self.codegen.emit(Instruction::LoadConst { dst, idx });
                }
                Ok(dst)
            }

            Expression::StringLiteral(lit) => {
                let dst = self.codegen.alloc_reg();
                let idx = self.codegen.add_string(&lit.value);
                self.codegen.emit(Instruction::LoadConst { dst, idx });
                Ok(dst)
            }

            Expression::BooleanLiteral(lit) => {
                let dst = self.codegen.alloc_reg();
                if lit.value {
                    self.codegen.emit(Instruction::LoadTrue { dst });
                } else {
                    self.codegen.emit(Instruction::LoadFalse { dst });
                }
                Ok(dst)
            }

            Expression::NullLiteral(_) => {
                let dst = self.codegen.alloc_reg();
                self.codegen.emit(Instruction::LoadNull { dst });
                Ok(dst)
            }

            Expression::Identifier(ident) => self.compile_identifier(&ident.name),

            Expression::BinaryExpression(binary) => self.compile_binary_expression(binary),

            Expression::UnaryExpression(unary) => self.compile_unary_expression(unary),

            Expression::AssignmentExpression(assign) => self.compile_assignment_expression(assign),

            Expression::CallExpression(call) => self.compile_call_expression(call),

            Expression::StaticMemberExpression(member) => {
                self.compile_static_member_expression(member)
            }

            Expression::ComputedMemberExpression(member) => {
                self.compile_computed_member_expression(member)
            }

            Expression::ObjectExpression(obj) => self.compile_object_expression(obj),

            Expression::ArrayExpression(arr) => self.compile_array_expression(arr),

            Expression::ConditionalExpression(cond) => self.compile_conditional_expression(cond),

            Expression::ParenthesizedExpression(paren) => {
                self.compile_expression(&paren.expression)
            }

            Expression::FunctionExpression(func) => self.compile_function_expression(func),

            Expression::ArrowFunctionExpression(arrow) => self.compile_arrow_function(arrow),

            Expression::NewExpression(new_expr) => self.compile_new_expression(new_expr),

            Expression::UpdateExpression(update) => self.compile_update_expression(update),

            _ => Err(CompileError::unsupported("Unknown expression type")),
        }
    }

    /// Compile an identifier reference
    fn compile_identifier(&mut self, name: &str) -> CompileResult<Register> {
        let dst = self.codegen.alloc_reg();

        match self.codegen.resolve_variable(name) {
            Some(ResolvedBinding::Local(idx)) => {
                self.codegen.emit(Instruction::GetLocal {
                    dst,
                    idx: LocalIndex(idx),
                });
            }
            Some(ResolvedBinding::Global(name)) => {
                let name_idx = self.codegen.add_string(&name);
                self.codegen.emit(Instruction::GetGlobal {
                    dst,
                    name: name_idx,
                });
            }
            Some(ResolvedBinding::Upvalue { .. }) => {
                return Err(CompileError::unsupported("Upvalues"));
            }
            None => {
                let name_idx = self.codegen.add_string(name);
                self.codegen.emit(Instruction::GetGlobal {
                    dst,
                    name: name_idx,
                });
            }
        }

        Ok(dst)
    }

    /// Compile a binary expression
    fn compile_binary_expression(&mut self, binary: &BinaryExpression) -> CompileResult<Register> {
        let lhs = self.compile_expression(&binary.left)?;
        let rhs = self.compile_expression(&binary.right)?;
        let dst = self.codegen.alloc_reg();

        let instruction = match binary.operator {
            BinaryOperator::Addition => Instruction::Add { dst, lhs, rhs },
            BinaryOperator::Subtraction => Instruction::Sub { dst, lhs, rhs },
            BinaryOperator::Multiplication => Instruction::Mul { dst, lhs, rhs },
            BinaryOperator::Division => Instruction::Div { dst, lhs, rhs },
            BinaryOperator::Remainder => Instruction::Mod { dst, lhs, rhs },
            BinaryOperator::LessThan => Instruction::Lt { dst, lhs, rhs },
            BinaryOperator::LessEqualThan => Instruction::Le { dst, lhs, rhs },
            BinaryOperator::GreaterThan => Instruction::Gt { dst, lhs, rhs },
            BinaryOperator::GreaterEqualThan => Instruction::Ge { dst, lhs, rhs },
            BinaryOperator::Equality => Instruction::Eq { dst, lhs, rhs },
            BinaryOperator::Inequality => Instruction::Ne { dst, lhs, rhs },
            BinaryOperator::StrictEquality => Instruction::StrictEq { dst, lhs, rhs },
            BinaryOperator::StrictInequality => Instruction::StrictNe { dst, lhs, rhs },
            BinaryOperator::BitwiseAnd => Instruction::BitAnd { dst, lhs, rhs },
            BinaryOperator::BitwiseOR => Instruction::BitOr { dst, lhs, rhs },
            BinaryOperator::BitwiseXOR => Instruction::BitXor { dst, lhs, rhs },
            BinaryOperator::ShiftLeft => Instruction::Shl { dst, lhs, rhs },
            BinaryOperator::ShiftRight => Instruction::Shr { dst, lhs, rhs },
            BinaryOperator::ShiftRightZeroFill => Instruction::Ushr { dst, lhs, rhs },
            _ => {
                return Err(CompileError::unsupported(format!(
                    "Binary operator: {:?}",
                    binary.operator
                )))
            }
        };

        self.codegen.emit(instruction);
        self.codegen.free_reg(lhs);
        self.codegen.free_reg(rhs);

        Ok(dst)
    }

    /// Compile a unary expression
    fn compile_unary_expression(&mut self, unary: &UnaryExpression) -> CompileResult<Register> {
        let src = self.compile_expression(&unary.argument)?;
        let dst = self.codegen.alloc_reg();

        let instruction = match unary.operator {
            UnaryOperator::UnaryNegation => Instruction::Neg { dst, src },
            UnaryOperator::LogicalNot => Instruction::Not { dst, src },
            UnaryOperator::BitwiseNot => Instruction::BitNot { dst, src },
            UnaryOperator::Typeof => Instruction::TypeOf { dst, src },
            _ => {
                return Err(CompileError::unsupported(format!(
                    "Unary operator: {:?}",
                    unary.operator
                )))
            }
        };

        self.codegen.emit(instruction);
        self.codegen.free_reg(src);

        Ok(dst)
    }

    /// Compile an assignment expression
    fn compile_assignment_expression(
        &mut self,
        assign: &AssignmentExpression,
    ) -> CompileResult<Register> {
        let value = self.compile_expression(&assign.right)?;

        match &assign.left {
            AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                match self.codegen.resolve_variable(&ident.name) {
                    Some(ResolvedBinding::Local(idx)) => {
                        self.codegen.emit(Instruction::SetLocal {
                            idx: LocalIndex(idx),
                            src: value,
                        });
                    }
                    Some(ResolvedBinding::Global(_)) | None => {
                        let name_idx = self.codegen.add_string(&ident.name);
                        self.codegen.emit(Instruction::SetGlobal {
                            name: name_idx,
                            src: value,
                        });
                    }
                    Some(ResolvedBinding::Upvalue { .. }) => {
                        return Err(CompileError::unsupported("Upvalue assignment"));
                    }
                }
            }
            AssignmentTarget::StaticMemberExpression(member) => {
                let obj = self.compile_expression(&member.object)?;
                let name_idx = self.codegen.add_string(&member.property.name);
                self.codegen.emit(Instruction::SetPropConst {
                    obj,
                    name: name_idx,
                    val: value,
                });
                self.codegen.free_reg(obj);
            }
            AssignmentTarget::ComputedMemberExpression(member) => {
                let obj = self.compile_expression(&member.object)?;
                let key = self.compile_expression(&member.expression)?;
                self.codegen.emit(Instruction::SetProp {
                    obj,
                    key,
                    val: value,
                });
                self.codegen.free_reg(key);
                self.codegen.free_reg(obj);
            }
            _ => return Err(CompileError::InvalidAssignmentTarget),
        }

        Ok(value)
    }

    /// Compile a call expression
    fn compile_call_expression(&mut self, call: &CallExpression) -> CompileResult<Register> {
        // Compile callee
        let func = self.compile_expression(&call.callee)?;

        // Compile arguments - collect them first
        let argc = call.arguments.len() as u8;
        let mut arg_regs = Vec::with_capacity(call.arguments.len());

        for arg in &call.arguments {
            match arg {
                Argument::SpreadElement(_) => {
                    return Err(CompileError::unsupported("Spread arguments"));
                }
                _ => {
                    let reg = self.compile_expression(arg.to_expression())?;
                    arg_regs.push(reg);
                }
            }
        }

        // Free argument registers after we're done with them
        for reg in arg_regs.iter().rev() {
            self.codegen.free_reg(*reg);
        }

        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Call { dst, func, argc });
        self.codegen.free_reg(func);

        Ok(dst)
    }

    /// Compile a new expression (new Foo(...))
    fn compile_new_expression(&mut self, new_expr: &NewExpression) -> CompileResult<Register> {
        // Compile callee (constructor)
        let func = self.compile_expression(&new_expr.callee)?;

        // Compile arguments
        let argc = new_expr.arguments.len() as u8;
        let mut arg_regs = Vec::with_capacity(new_expr.arguments.len());

        for arg in &new_expr.arguments {
            match arg {
                Argument::SpreadElement(_) => {
                    return Err(CompileError::unsupported("Spread arguments"));
                }
                _ => {
                    let reg = self.compile_expression(arg.to_expression())?;
                    arg_regs.push(reg);
                }
            }
        }

        // Free argument registers
        for reg in arg_regs.iter().rev() {
            self.codegen.free_reg(*reg);
        }

        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::Construct { dst, func, argc });
        self.codegen.free_reg(func);

        Ok(dst)
    }

    /// Compile an update expression (i++, ++i, i--, --i)
    fn compile_update_expression(&mut self, update: &UpdateExpression) -> CompileResult<Register> {
        // Get the argument (must be an identifier or member expression)
        let argument = &update.argument;

        match argument {
            SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
                return self.compile_update_identifier(ident, update.operator, update.prefix);
            }
            _ => {
                return Err(CompileError::unsupported(
                    "Update expression on non-identifier",
                ));
            }
        }
    }

    /// Compile update on identifier
    fn compile_update_identifier(
        &mut self,
        ident: &IdentifierReference,
        operator: oxc_ast::ast::UpdateOperator,
        prefix: bool,
    ) -> CompileResult<Register> {
        let name = &ident.name;

        // Load current value
        let current = self.compile_identifier(name)?;

        // Result register
        let result = self.codegen.alloc_reg();

        if prefix {
            // Prefix: ++i or --i
            match operator {
                UpdateOperator::Increment => {
                    self.codegen.emit(Instruction::Inc {
                        dst: result,
                        src: current,
                    });
                }
                UpdateOperator::Decrement => {
                    self.codegen.emit(Instruction::Dec {
                        dst: result,
                        src: current,
                    });
                }
            }
            // Store back to variable
            self.store_to_identifier(name, result)?;
        } else {
            // Postfix: i++ or i--
            // First copy current value as result
            self.codegen.emit(Instruction::Move {
                dst: result,
                src: current,
            });

            // Increment/decrement in a temp
            let new_val = self.codegen.alloc_reg();
            match operator {
                UpdateOperator::Increment => {
                    self.codegen.emit(Instruction::Inc {
                        dst: new_val,
                        src: current,
                    });
                }
                UpdateOperator::Decrement => {
                    self.codegen.emit(Instruction::Dec {
                        dst: new_val,
                        src: current,
                    });
                }
            }
            // Store new value back
            self.store_to_identifier(name, new_val)?;
            self.codegen.free_reg(new_val);
        }

        self.codegen.free_reg(current);
        Ok(result)
    }

    /// Store a value to an identifier (variable)
    fn store_to_identifier(&mut self, name: &str, src: Register) -> CompileResult<()> {
        match self.codegen.resolve_variable(name) {
            Some(ResolvedBinding::Local(idx)) => {
                self.codegen.emit(Instruction::SetLocal {
                    idx: LocalIndex(idx),
                    src,
                });
            }
            Some(ResolvedBinding::Global(name)) => {
                let name_idx = self.codegen.add_string(&name);
                self.codegen.emit(Instruction::SetGlobal { name: name_idx, src });
            }
            Some(ResolvedBinding::Upvalue { .. }) => {
                return Err(CompileError::unsupported("Upvalues"));
            }
            None => {
                // Undeclared variable - treat as global
                let name_idx = self.codegen.add_string(name);
                self.codegen.emit(Instruction::SetGlobal { name: name_idx, src });
            }
        }
        Ok(())
    }

    /// Compile a static member expression (obj.prop)
    fn compile_static_member_expression(
        &mut self,
        member: &StaticMemberExpression,
    ) -> CompileResult<Register> {
        let obj = self.compile_expression(&member.object)?;
        let dst = self.codegen.alloc_reg();
        let name_idx = self.codegen.add_string(&member.property.name);
        self.codegen.emit(Instruction::GetPropConst {
            dst,
            obj,
            name: name_idx,
        });
        self.codegen.free_reg(obj);
        Ok(dst)
    }

    /// Compile a computed member expression (obj[key])
    fn compile_computed_member_expression(
        &mut self,
        member: &ComputedMemberExpression,
    ) -> CompileResult<Register> {
        let obj = self.compile_expression(&member.object)?;
        let dst = self.codegen.alloc_reg();
        let key = self.compile_expression(&member.expression)?;
        self.codegen.emit(Instruction::GetProp { dst, obj, key });
        self.codegen.free_reg(key);
        self.codegen.free_reg(obj);
        Ok(dst)
    }

    /// Compile an object expression
    fn compile_object_expression(&mut self, obj: &ObjectExpression) -> CompileResult<Register> {
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::NewObject { dst });

        for prop in &obj.properties {
            match prop {
                ObjectPropertyKind::ObjectProperty(prop) => {
                    let key = match &prop.key {
                        PropertyKey::StaticIdentifier(ident) => self.codegen.add_string(&ident.name),
                        PropertyKey::StringLiteral(lit) => self.codegen.add_string(&lit.value),
                        _ => return Err(CompileError::unsupported("Computed property keys")),
                    };

                    let value = self.compile_expression(&prop.value)?;
                    self.codegen.emit(Instruction::SetPropConst {
                        obj: dst,
                        name: key,
                        val: value,
                    });
                    self.codegen.free_reg(value);
                }
                ObjectPropertyKind::SpreadProperty(_) => {
                    return Err(CompileError::unsupported("Object spread"));
                }
            }
        }

        Ok(dst)
    }

    /// Compile an array expression
    fn compile_array_expression(&mut self, arr: &ArrayExpression) -> CompileResult<Register> {
        let len = arr.elements.len() as u16;
        let dst = self.codegen.alloc_reg();
        self.codegen.emit(Instruction::NewArray { dst, len });

        for (i, elem) in arr.elements.iter().enumerate() {
            match elem {
                ArrayExpressionElement::SpreadElement(_) => {
                    return Err(CompileError::unsupported("Array spread"));
                }
                ArrayExpressionElement::Elision(_) => {
                    // Skip - already undefined
                }
                _ => {
                    let value = self.compile_expression(elem.to_expression())?;
                    let idx_reg = self.codegen.alloc_reg();
                    self.codegen.emit(Instruction::LoadInt32 {
                        dst: idx_reg,
                        value: i as i32,
                    });
                    self.codegen.emit(Instruction::SetElem {
                        arr: dst,
                        idx: idx_reg,
                        val: value,
                    });
                    self.codegen.free_reg(idx_reg);
                    self.codegen.free_reg(value);
                }
            }
        }

        Ok(dst)
    }

    /// Compile a conditional (ternary) expression
    fn compile_conditional_expression(
        &mut self,
        cond: &ConditionalExpression,
    ) -> CompileResult<Register> {
        let test = self.compile_expression(&cond.test)?;
        let jump_else = self.codegen.emit_jump_if_false(test);
        self.codegen.free_reg(test);

        // Compile consequent
        let result = self.compile_expression(&cond.consequent)?;
        let jump_end = self.codegen.emit_jump();

        // Patch jump to else
        let else_offset = self.codegen.current_index() as i32 - jump_else as i32;
        self.codegen.patch_jump(jump_else, else_offset);

        // Compile alternate into same register
        self.codegen.free_reg(result);
        let alt = self.compile_expression(&cond.alternate)?;

        // Move to result register if different
        if alt.0 != result.0 {
            self.codegen.emit(Instruction::Move {
                dst: result,
                src: alt,
            });
            self.codegen.free_reg(alt);
        }

        // Patch jump to end
        let end_offset = self.codegen.current_index() as i32 - jump_end as i32;
        self.codegen.patch_jump(jump_end, end_offset);

        Ok(result)
    }
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compile_number() {
        let compiler = Compiler::new();
        let module = compiler.compile("42", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_addition() {
        let compiler = Compiler::new();
        let module = compiler.compile("1 + 2", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_variable() {
        let compiler = Compiler::new();
        let module = compiler.compile("let x = 10; x + 5", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_if() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("if (true) { 1 } else { 2 }", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_while() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let i = 0; while (i < 10) { i = i + 1 }", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_const() {
        let compiler = Compiler::new();
        let module = compiler.compile("const PI = 3.15;", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_multiple_variables() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let a = 1; let b = 2; const c = a + b;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_if_else() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let x = 5; if (x > 10) { x = 1; } else { x = 2; }",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_for() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let sum = 0; for (let i = 0; i < 10; i = i + 1) { sum = sum + i; }",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_block_scope() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let x = 1; { let y = 2; x = x + y; }", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_function_declaration() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("function add(a, b) { return a + b; }", "test.js")
            .unwrap();

        // 2 functions: main + add
        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_function_call() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "function double(x) { return x * 2; } let result = double(5);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_function_expression() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let add = function(a, b) { return a + b; }; add(1, 2);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_arrow_function() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let add = (a, b) => a + b; let result = add(2, 3);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_arrow_function_block() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "let add = (a, b) => { return a + b; }; add(1, 2);",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_recursion() {
        let compiler = Compiler::new();
        let module = compiler
            .compile(
                "function factorial(n) { if (n <= 1) { return 1; } return n * factorial(n - 1); }",
                "test.js",
            )
            .unwrap();

        assert_eq!(module.functions.len(), 2);
    }

    #[test]
    fn test_compile_object_literal() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let obj = { x: 1, y: 2 };", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_array_literal() {
        let compiler = Compiler::new();
        let module = compiler.compile("let arr = [1, 2, 3];", "test.js").unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_property_access() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let obj = { x: 10 }; let v = obj.x;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_element_access() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let arr = [1, 2, 3]; let v = arr[1];", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_property_assignment() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let obj = { x: 1 }; obj.x = 42;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }

    #[test]
    fn test_compile_element_assignment() {
        let compiler = Compiler::new();
        let module = compiler
            .compile("let arr = [1, 2, 3]; arr[0] = 10;", "test.js")
            .unwrap();

        assert_eq!(module.functions.len(), 1);
    }
}
