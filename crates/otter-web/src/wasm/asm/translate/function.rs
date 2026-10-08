//! asm.js function bodies (§6.4–6.5): parameter annotations, locals and
//! statements, emitted as buffered wasm with blocks for every loop, label
//! and switch (V8's block stack).
//!
//! # Invariants
//! - `break` targets the innermost loop or switch block, or the labeled
//!   one; `continue` the innermost or labeled loop.
//! - Temporaries are wasm locals past the declared ones, reused by type.

use super::*;

mod expression;

// ---------------------------------------------------------------------------
// Function bodies
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    /// `break` target without a label; with one when labeled.
    Regular,
    /// `continue` target.
    Loop,
    /// A labeled block: a labeled `break` target only.
    Named,
    /// `if` arms and switch case blocks: never a target.
    Other,
}

struct BlockInfo<'a> {
    kind: BlockKind,
    label: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct LocalInfo {
    ty: AsmType,
    index: u32,
}

/// How the context of a call coerces its result (V8 `call_coercion_`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Coercion {
    None,
    Signed,
    Double,
    Float,
}

impl Coercion {
    fn result(self) -> AsmType {
        match self {
            Self::None => AsmType::VOID,
            Self::Signed => AsmType::SIGNED,
            Self::Double => AsmType::DOUBLE,
            Self::Float => AsmType::FLOAT,
        }
    }
}

pub(super) struct FunctionTranslator<'m, 'a> {
    module: &'m mut ModuleTranslator<'a>,
    locals: HashMap<&'a str, LocalInfo>,
    param_count: u32,
    local_types: Vec<ValType>,
    free_temps: Vec<(ValType, u32)>,
    ops: Vec<Op>,
    blocks: Vec<BlockInfo<'a>>,
    pub(super) return_type: Option<AsmType>,
    pending_label: Option<&'a str>,
    /// `int` terms of the last integer additive expression emitted.
    additive_terms: u32,
}

impl<'m, 'a> FunctionTranslator<'m, 'a> {
    pub(super) fn new(module: &'m mut ModuleTranslator<'a>, param_count: u32) -> Self {
        Self {
            module,
            locals: HashMap::new(),
            param_count,
            local_types: Vec::new(),
            free_temps: Vec::new(),
            ops: Vec::new(),
            blocks: Vec::new(),
            return_type: None,
            pending_label: None,
            additive_terms: 1,
        }
    }

    pub(super) fn finish(self) -> (Vec<ValType>, Vec<Op>) {
        (self.local_types, self.ops)
    }

    fn emit(&mut self, instruction: Instruction<'static>) {
        self.ops.push(Op::Wasm(instruction));
    }

    fn call(&mut self, callee: Callee) {
        self.ops.push(Op::Call(callee));
    }

    fn new_local(&mut self, ty: ValType) -> u32 {
        self.local_types.push(ty);
        self.param_count + self.local_types.len() as u32 - 1
    }

    fn temp(&mut self, ty: ValType) -> u32 {
        if let Some(position) = self.free_temps.iter().rposition(|(t, _)| *t == ty) {
            return self.free_temps.swap_remove(position).1;
        }
        self.new_local(ty)
    }

    fn release(&mut self, ty: ValType, index: u32) {
        self.free_temps.push((ty, index));
    }

    pub(super) fn declare_local(&mut self, name: &'a str, ty: AsmType, index: u32) -> Result<()> {
        if self.locals.insert(name, LocalInfo { ty, index }).is_some() {
            return fail("duplicate local");
        }
        Ok(())
    }

    // -- 5.1 Parameter annotations ------------------------------------------

    pub(super) fn parameter_annotation(
        &self,
        name: &str,
        expression: &Expression<'a>,
    ) -> Result<AsmType> {
        let Expression::AssignmentExpression(assignment) = expression else {
            return fail("bad parameter annotation");
        };
        let AssignmentTarget::AssignmentTargetIdentifier(target) = &assignment.left else {
            return fail("bad parameter annotation");
        };
        if assignment.operator != AssignmentOperator::Assign || target.name.as_str() != name {
            return fail("bad parameter annotation");
        }
        let is_param = |expression: &Expression<'_>| matches!(expression, Expression::Identifier(id) if id.name.as_str() == name);
        match &assignment.right {
            Expression::BinaryExpression(binary)
                if binary.operator == BinaryOperator::BitwiseOR
                    && is_param(&binary.left)
                    && is_zero(&binary.right) =>
            {
                Ok(AsmType::INT)
            }
            Expression::UnaryExpression(unary)
                if unary.operator == UnaryOperator::UnaryPlus && is_param(&unary.argument) =>
            {
                Ok(AsmType::DOUBLE)
            }
            Expression::CallExpression(call) if self.module.is_fround(&call.callee) => {
                match call.arguments.as_slice() {
                    [Argument::Identifier(id)] if id.name.as_str() == name => Ok(AsmType::FLOAT),
                    _ => fail("bad parameter annotation"),
                }
            }
            _ => fail("bad parameter annotation"),
        }
    }

    // -- 6.4 locals ---------------------------------------------------------

    pub(super) fn local_variables(
        &mut self,
        declaration: &'a VariableDeclaration<'a>,
    ) -> Result<()> {
        if declaration.kind != oxc_ast::ast::VariableDeclarationKind::Var {
            return fail("local must be a var");
        }
        for declarator in &declaration.declarations {
            let BindingPattern::BindingIdentifier(id) = &declarator.id else {
                return fail("bad local");
            };
            let Some(init) = &declarator.init else {
                return fail("local without initializer");
            };
            let (ty, val, value) = self.local_initializer(init)?;
            let index = self.new_local(val);
            self.declare_local(id.name.as_str(), ty, index)?;
            // Wasm locals start at zero; only other initial values are set.
            if let Some(value) = value {
                self.emit(value);
                self.emit(Instruction::LocalSet(index));
            }
        }
        Ok(())
    }

    fn local_initializer(
        &mut self,
        init: &'a Expression<'a>,
    ) -> Result<(AsmType, ValType, Option<Instruction<'static>>)> {
        let nonzero_i32 = |value: i32| (value != 0).then_some(Instruction::I32Const(value));
        let nonzero_f64 =
            |value: f64| (value.to_bits() != 0).then_some(Instruction::F64Const(value.into()));
        match init {
            Expression::NumericLiteral(literal) => Ok(match classify(literal)? {
                Literal::Double(value) => (AsmType::DOUBLE, ValType::F64, nonzero_f64(value)),
                Literal::Unsigned(value) => (AsmType::INT, ValType::I32, nonzero_i32(value as i32)),
            }),
            Expression::UnaryExpression(unary)
                if unary.operator == UnaryOperator::UnaryNegation =>
            {
                let Expression::NumericLiteral(literal) = &unary.argument else {
                    return fail("bad local initializer");
                };
                Ok(match classify(literal)? {
                    Literal::Double(value) => (AsmType::DOUBLE, ValType::F64, nonzero_f64(-value)),
                    Literal::Unsigned(value) if value <= 0x7FFF_FFFF => {
                        (AsmType::INT, ValType::I32, nonzero_i32(-(value as i32)))
                    }
                    Literal::Unsigned(_) => return fail("numeric literal out of range"),
                })
            }
            Expression::Identifier(id) => match self.module.names.get(id.name.as_str()).copied() {
                Some(Binding::Global {
                    ty,
                    slot,
                    mutable: false,
                }) => {
                    let Some(val) = ty.val_type().filter(|_| is_value_type(ty)) else {
                        return fail("bad local initializer");
                    };
                    let index = self.module.global_index(slot);
                    Ok((ty, val, Some(Instruction::GlobalGet(index))))
                }
                Some(Binding::Constant(value)) => {
                    Ok((AsmType::DOUBLE, ValType::F64, nonzero_f64(value)))
                }
                _ => fail("bad local initializer"),
            },
            Expression::CallExpression(call) if self.module.is_fround(&call.callee) => {
                let value = fround_literal(call)?;
                let set = (value.to_bits() != 0).then_some(Instruction::F32Const(value.into()));
                Ok((AsmType::FLOAT, ValType::F32, set))
            }
            _ => fail("bad local initializer"),
        }
    }

    // -- 6.5 ValidateStatement ----------------------------------------------

    pub(super) fn statement(&mut self, statement: &'a Statement<'a>) -> Result<()> {
        let label = self.pending_label.take();
        match statement {
            Statement::BlockStatement(block) => {
                if label.is_some() {
                    self.begin(
                        BlockKind::Named,
                        label,
                        Instruction::Block(BlockType::Empty),
                    );
                }
                for statement in &block.body {
                    self.statement(statement)?;
                }
                if label.is_some() {
                    self.end();
                }
                Ok(())
            }
            Statement::EmptyStatement(_) => Ok(()),
            Statement::ExpressionStatement(statement) if label.is_none() => {
                let ty = self.expression(&statement.expression)?;
                if ty != AsmType::VOID {
                    self.emit(Instruction::Drop);
                }
                Ok(())
            }
            Statement::IfStatement(statement) if label.is_none() => {
                self.expect(&statement.test, AsmType::INT)?;
                self.begin(BlockKind::Other, None, Instruction::If(BlockType::Empty));
                self.statement(&statement.consequent)?;
                if let Some(alternate) = &statement.alternate {
                    self.emit(Instruction::Else);
                    self.statement(alternate)?;
                }
                self.end();
                Ok(())
            }
            Statement::ReturnStatement(statement) if label.is_none() => {
                self.return_statement(statement.argument.as_ref())
            }
            Statement::WhileStatement(statement) => {
                // a: block { b: loop { if (!test) break a; body; continue b; } }
                self.begin(
                    BlockKind::Regular,
                    label,
                    Instruction::Block(BlockType::Empty),
                );
                self.begin(BlockKind::Loop, label, Instruction::Loop(BlockType::Empty));
                self.expect(&statement.test, AsmType::INT)?;
                self.emit(Instruction::I32Eqz);
                self.emit(Instruction::BrIf(1));
                self.statement(&statement.body)?;
                self.emit(Instruction::Br(0));
                self.end();
                self.end();
                Ok(())
            }
            Statement::DoWhileStatement(statement) => {
                // a: block { b: loop { c: block { body } if (!test) break a; continue b; } }
                self.begin(
                    BlockKind::Regular,
                    label,
                    Instruction::Block(BlockType::Empty),
                );
                self.begin(BlockKind::Loop, None, Instruction::Loop(BlockType::Empty));
                self.begin(BlockKind::Loop, label, Instruction::Block(BlockType::Empty));
                self.statement(&statement.body)?;
                self.end();
                self.expect(&statement.test, AsmType::INT)?;
                self.emit(Instruction::I32Eqz);
                self.emit(Instruction::BrIf(1));
                self.emit(Instruction::Br(0));
                self.end();
                self.end();
                Ok(())
            }
            Statement::ForStatement(statement) => {
                if let Some(init) = &statement.init {
                    let Some(init) = init_expression(init) else {
                        return fail("for initializer must be an expression");
                    };
                    let ty = self.expression(init)?;
                    if ty != AsmType::VOID {
                        self.emit(Instruction::Drop);
                    }
                }
                // a: block { b: loop { c: block { if (!test) break a; body } update; continue b; } }
                self.begin(
                    BlockKind::Regular,
                    label,
                    Instruction::Block(BlockType::Empty),
                );
                self.begin(BlockKind::Loop, None, Instruction::Loop(BlockType::Empty));
                self.begin(BlockKind::Loop, label, Instruction::Block(BlockType::Empty));
                if let Some(test) = &statement.test {
                    self.expect(test, AsmType::INT)?;
                    self.emit(Instruction::I32Eqz);
                    self.emit(Instruction::BrIf(2));
                }
                self.statement(&statement.body)?;
                self.end();
                if let Some(update) = &statement.update {
                    let ty = self.expression(update)?;
                    if ty != AsmType::VOID {
                        self.emit(Instruction::Drop);
                    }
                }
                self.emit(Instruction::Br(0));
                self.end();
                self.end();
                Ok(())
            }
            Statement::BreakStatement(statement) if label.is_none() => {
                let target = statement.label.as_ref().map(|label| label.name.as_str());
                let depth = self
                    .blocks
                    .iter()
                    .rev()
                    .position(|block| match (block.kind, target) {
                        (BlockKind::Regular, None) => true,
                        (BlockKind::Regular | BlockKind::Named, Some(target)) => {
                            block.label == Some(target)
                        }
                        _ => false,
                    })
                    .ok_or(Invalid("illegal break"))?;
                self.emit(Instruction::Br(depth as u32));
                Ok(())
            }
            Statement::ContinueStatement(statement) if label.is_none() => {
                let target = statement.label.as_ref().map(|label| label.name.as_str());
                let depth = self
                    .blocks
                    .iter()
                    .rev()
                    .position(|block| {
                        block.kind == BlockKind::Loop && (target.is_none() || block.label == target)
                    })
                    .ok_or(Invalid("illegal continue"))?;
                self.emit(Instruction::Br(depth as u32));
                Ok(())
            }
            Statement::LabeledStatement(statement) if label.is_none() => {
                match &statement.body {
                    Statement::BlockStatement(_)
                    | Statement::WhileStatement(_)
                    | Statement::DoWhileStatement(_)
                    | Statement::ForStatement(_)
                    | Statement::SwitchStatement(_) => {}
                    _ => return fail("label on a statement that is not a block, loop or switch"),
                }
                self.pending_label = Some(statement.label.name.as_str());
                self.statement(&statement.body)
            }
            Statement::SwitchStatement(statement) => self.switch(statement, label),
            _ => fail("statement outside asm.js"),
        }
    }

    fn begin(
        &mut self,
        kind: BlockKind,
        label: Option<&'a str>,
        instruction: Instruction<'static>,
    ) {
        self.blocks.push(BlockInfo { kind, label });
        self.emit(instruction);
    }

    fn end(&mut self) {
        self.blocks.pop();
        self.emit(Instruction::End);
    }

    fn return_statement(&mut self, argument: Option<&'a Expression<'a>>) -> Result<()> {
        match argument {
            Some(argument) => {
                let ty = self.expression(argument)?;
                if let Some(expected) = self.return_type
                    && !ty.is_a(expected)
                {
                    return fail("return type mismatch");
                }
                self.return_type = Some(if ty.is_a(AsmType::DOUBLE) {
                    AsmType::DOUBLE
                } else if ty.is_a(AsmType::FLOAT) {
                    AsmType::FLOAT
                } else if ty.is_a(AsmType::SIGNED) {
                    AsmType::SIGNED
                } else {
                    return fail("invalid return type");
                });
            }
            None => match self.return_type {
                None => self.return_type = Some(AsmType::VOID),
                Some(ty) if ty == AsmType::VOID => {}
                Some(_) => return fail("invalid void return"),
            },
        }
        self.emit(Instruction::Return);
        Ok(())
    }

    // -- 6.5.10 SwitchStatement ---------------------------------------------

    fn switch(&mut self, statement: &'a SwitchStatement<'a>, label: Option<&'a str>) -> Result<()> {
        let ty = self.expression(&statement.discriminant)?;
        if !ty.is_a(AsmType::SIGNED) {
            return fail("expected signed for switch value");
        }
        let tag = self.temp(ValType::I32);
        self.emit(Instruction::LocalSet(tag));
        let mut values = Vec::new();
        for (index, case) in statement.cases.iter().enumerate() {
            match &case.test {
                Some(test) => values.push(case_value(test)?),
                None if index + 1 == statement.cases.len() => {}
                None => return fail("default must be the last clause"),
            }
        }
        let has_default = statement
            .cases
            .last()
            .is_some_and(|case| case.test.is_none());
        // end: block { default: block { case_n: block { ... case_0: block {
        //   dispatch } body_0 } ... body_n } default_body }
        self.begin(
            BlockKind::Regular,
            label,
            Instruction::Block(BlockType::Empty),
        );
        for _ in 0..=values.len() {
            self.begin(BlockKind::Other, None, Instruction::Block(BlockType::Empty));
        }
        self.dispatch(tag, &values);
        self.release(ValType::I32, tag);
        // Each clause starts where its block ends; the default clause after
        // the last case block.
        for case in &statement.cases {
            self.end();
            for statement in &case.consequent {
                self.statement(statement)?;
            }
        }
        if !has_default {
            self.end();
        }
        self.end();
        Ok(())
    }

    /// Branch to the block of the first case equal to `tag`, else past the
    /// last case block. Dense cases use one `br_table`.
    fn dispatch(&mut self, tag: u32, values: &[i32]) {
        let count = values.len() as u32;
        if values.is_empty() {
            self.emit(Instruction::Br(0));
            return;
        }
        let min = *values.iter().min().expect("non-empty");
        let max = *values.iter().max().expect("non-empty");
        let span = i64::from(max) - i64::from(min) + 1;
        if values.len() >= 4 && span <= 2 * values.len() as i64 + 8 {
            let mut targets = vec![count; span as usize];
            // The first matching case wins, as it does in JavaScript.
            for (position, value) in values.iter().enumerate().rev() {
                targets[(i64::from(*value) - i64::from(min)) as usize] = position as u32;
            }
            self.emit(Instruction::LocalGet(tag));
            if min != 0 {
                self.emit(Instruction::I32Const(min));
                self.emit(Instruction::I32Sub);
            }
            self.emit(Instruction::BrTable(Cow::Owned(targets), count));
            return;
        }
        for (position, value) in values.iter().enumerate() {
            self.emit(Instruction::LocalGet(tag));
            self.emit(Instruction::I32Const(*value));
            self.emit(Instruction::I32Eq);
            self.emit(Instruction::BrIf(position as u32));
        }
        self.emit(Instruction::Br(count));
    }
}
