//! asm.js expressions (§6.8–6.11): typing, heap access, operators, calls
//! and coercions.
//!
//! # Invariants
//! - Heap loads past the heap yield the typed-array miss value and stores
//!   past it are dropped; integer division never traps.

use super::*;

impl<'m, 'a> FunctionTranslator<'m, 'a> {
    // -- 6.8 Expressions ----------------------------------------------------

    /// Validate `expression` and require its type to be a subtype of `ty`.
    pub(super) fn expect(&mut self, expression: &'a Expression<'a>, ty: AsmType) -> Result<()> {
        if !self.expression(expression)?.is_a(ty) {
            return fail("unexpected type");
        }
        Ok(())
    }

    pub(super) fn expression(&mut self, expression: &'a Expression<'a>) -> Result<AsmType> {
        match expression {
            Expression::NumericLiteral(literal) => match classify(literal)? {
                Literal::Double(value) => {
                    self.emit(Instruction::F64Const(value.into()));
                    Ok(AsmType::DOUBLE)
                }
                Literal::Unsigned(value) => {
                    self.emit(Instruction::I32Const(value as i32));
                    Ok(if value <= 0x7FFF_FFFF {
                        AsmType::FIXNUM
                    } else {
                        AsmType::UNSIGNED
                    })
                }
            },
            Expression::Identifier(id) => self.identifier(id.name.as_str()),
            Expression::ParenthesizedExpression(inner) => self.expression(&inner.expression),
            Expression::SequenceExpression(sequence) => {
                let mut ty = AsmType::VOID;
                for (index, expression) in sequence.expressions.iter().enumerate() {
                    if index > 0 && ty != AsmType::VOID {
                        self.emit(Instruction::Drop);
                    }
                    ty = self.expression(expression)?;
                }
                Ok(ty)
            }
            Expression::AssignmentExpression(assignment) => {
                if assignment.operator != AssignmentOperator::Assign {
                    return fail("compound assignment");
                }
                match &assignment.left {
                    AssignmentTarget::AssignmentTargetIdentifier(target) => {
                        self.assign_variable(target.name.as_str(), &assignment.right)
                    }
                    AssignmentTarget::ComputedMemberExpression(member) => {
                        self.heap_store(member, &assignment.right)
                    }
                    _ => fail("invalid assignment target"),
                }
            }
            Expression::ComputedMemberExpression(member) => self.heap_load(member),
            Expression::CallExpression(call) => self.call_expression(call, Coercion::None),
            Expression::UnaryExpression(unary) => self.unary(unary.operator, &unary.argument),
            Expression::BinaryExpression(binary) => {
                self.binary(binary.operator, &binary.left, &binary.right)
            }
            Expression::ConditionalExpression(conditional) => {
                self.expect(&conditional.test, AsmType::INT)?;
                let at = self.ops.len();
                self.emit(Instruction::If(BlockType::Empty));
                let consequent = self.expression(&conditional.consequent)?;
                self.emit(Instruction::Else);
                let alternate = self.expression(&conditional.alternate)?;
                self.emit(Instruction::End);
                let (ty, val) = if consequent.is_a(AsmType::INT) && alternate.is_a(AsmType::INT) {
                    (AsmType::INT, ValType::I32)
                } else if consequent.is_a(AsmType::DOUBLE) && alternate.is_a(AsmType::DOUBLE) {
                    (AsmType::DOUBLE, ValType::F64)
                } else if consequent.is_a(AsmType::FLOAT) && alternate.is_a(AsmType::FLOAT) {
                    (AsmType::FLOAT, ValType::F32)
                } else {
                    return fail("type mismatch in ternary operator");
                };
                self.ops[at] = Op::Wasm(Instruction::If(BlockType::Result(val)));
                Ok(ty)
            }
            _ => fail("expression outside asm.js"),
        }
    }

    pub(super) fn identifier(&mut self, name: &'a str) -> Result<AsmType> {
        if let Some(local) = self.locals.get(name).copied() {
            self.emit(Instruction::LocalGet(local.index));
            return Ok(local.ty);
        }
        match self.module.names.get(name).copied() {
            Some(Binding::Global { ty, slot, .. }) => {
                let index = self.module.global_index(slot);
                self.emit(Instruction::GlobalGet(index));
                Ok(ty)
            }
            Some(Binding::Constant(value)) => {
                self.emit(Instruction::F64Const(value.into()));
                Ok(AsmType::DOUBLE)
            }
            _ => fail("undefined variable"),
        }
    }

    pub(super) fn assign_variable(
        &mut self,
        name: &'a str,
        value: &'a Expression<'a>,
    ) -> Result<AsmType> {
        if let Some(local) = self.locals.get(name).copied() {
            let ty = self.expression(value)?;
            if !ty.is_a(local.ty) {
                return fail("type mismatch in assignment");
            }
            self.emit(Instruction::LocalTee(local.index));
            return Ok(local.ty);
        }
        match self.module.names.get(name).copied() {
            Some(Binding::Global {
                ty: global_ty,
                slot,
                mutable: true,
            }) => {
                let ty = self.expression(value)?;
                if !ty.is_a(global_ty) {
                    return fail("type mismatch in assignment");
                }
                let index = self.module.global_index(slot);
                self.emit(Instruction::GlobalSet(index));
                self.emit(Instruction::GlobalGet(index));
                Ok(global_ty)
            }
            _ => fail("invalid assignment target"),
        }
    }

    // -- 6.10 ValidateHeapAccess --------------------------------------------

    /// Emit the byte address of `member` and return its view, or the view and
    /// a constant address.
    pub(super) fn heap_address(
        &mut self,
        member: &'a ComputedMemberExpression<'a>,
    ) -> Result<(HeapView, Option<u32>)> {
        if member.optional {
            return fail("optional heap access");
        }
        let Expression::Identifier(id) = &member.object else {
            return fail("expected heap view");
        };
        if self.locals.contains_key(id.name.as_str()) {
            return fail("expected heap view");
        }
        let Some(Binding::View(view)) = self.module.names.get(id.name.as_str()).copied() else {
            return fail("expected heap view");
        };
        let size = view.size();
        if let Some(offset) = unsigned_literal(&member.expression) {
            let address = u64::from(offset) * u64::from(size);
            if offset > 0x7FFF_FFFF || address > 0x7FFF_FFFF {
                return fail("heap access out of range");
            }
            return Ok((view, Some(address as u32)));
        }
        let index = if size == 1 {
            self.expression(&member.expression)?
        } else {
            // `index >> log2(size)` addresses byte `index & ~(size - 1)`.
            let Expression::BinaryExpression(shift) = &member.expression else {
                return fail("expected shift of word size");
            };
            if shift.operator != BinaryOperator::ShiftRight
                || unsigned_literal(&shift.right) != Some(size.trailing_zeros())
            {
                return fail("expected heap access shift to match view");
            }
            let ty = self.expression(&shift.left)?;
            self.emit(Instruction::I32Const(!(size as i32 - 1)));
            self.emit(Instruction::I32And);
            ty
        };
        if !index.is_a(AsmType::INTISH) {
            return fail("expected intish index");
        }
        Ok((view, None))
    }

    pub(super) fn heap_load(
        &mut self,
        member: &'a ComputedMemberExpression<'a>,
    ) -> Result<AsmType> {
        let (view, constant) = self.heap_address(member)?;
        let size = view.size();
        let limit = self.module.heap_limit(size)?;
        match constant {
            Some(address) if address < limit => {
                self.emit(Instruction::I32Const(address as i32));
                self.emit(load_instruction(view));
            }
            Some(_) => self.emit(out_of_bounds_value(view)),
            // Past the heap the reservation reads zero: the value an integer
            // view's miss (`undefined`) coerces to.
            None if self.module.reads_zero_past_heap() && view.load_type() == AsmType::INTISH => {
                self.emit(load_instruction(view));
            }
            // A float view's miss is NaN: select it, without branching, over
            // the zero the reservation reads.
            None if self.module.reads_zero_past_heap() => {
                let address = self.temp(ValType::I32);
                self.emit(Instruction::LocalTee(address));
                self.emit(load_instruction(view));
                self.emit(out_of_bounds_value(view));
                self.emit(Instruction::LocalGet(address));
                self.emit(Instruction::I32Const(limit as i32));
                self.emit(Instruction::I32LtU);
                self.emit(Instruction::Select);
                self.release(ValType::I32, address);
            }
            None => {
                let address = self.temp(ValType::I32);
                self.emit(Instruction::LocalTee(address));
                self.emit(Instruction::I32Const(limit as i32));
                self.emit(Instruction::I32LtU);
                self.emit(Instruction::If(BlockType::Result(view_val_type(view))));
                self.emit(Instruction::LocalGet(address));
                self.emit(load_instruction(view));
                self.emit(Instruction::Else);
                self.emit(out_of_bounds_value(view));
                self.emit(Instruction::End);
                self.release(ValType::I32, address);
            }
        }
        self.module.memory = true;
        Ok(view.load_type())
    }

    pub(super) fn heap_store(
        &mut self,
        member: &'a ComputedMemberExpression<'a>,
        value: &'a Expression<'a>,
    ) -> Result<AsmType> {
        let (view, constant) = self.heap_address(member)?;
        let size = view.size();
        let limit = self.module.heap_limit(size)?;
        let address = match constant {
            Some(_) => None,
            None => {
                let address = self.temp(ValType::I32);
                self.emit(Instruction::LocalSet(address));
                Some(address)
            }
        };
        let ty = self.expression(value)?;
        if !ty.is_a(view.store_type()) {
            return fail("illegal type stored to heap view");
        }
        // The assignment's value is the right-hand side as written; the view
        // converts only what it stores.
        let Some(value_val) = ty.val_type() else {
            return fail("illegal type stored to heap view");
        };
        let original = self.temp(value_val);
        let conversion = match view {
            HeapView::Float32 if ty.is_a(AsmType::DOUBLEQ) => Some(Instruction::F32DemoteF64),
            HeapView::Float64 if ty.is_a(AsmType::FLOATQ) => Some(Instruction::F64PromoteF32),
            _ => None,
        };
        let stored = match conversion {
            Some(conversion) => {
                self.emit(Instruction::LocalTee(original));
                self.emit(conversion);
                let stored = self.temp(view_val_type(view));
                self.emit(Instruction::LocalSet(stored));
                Some(stored)
            }
            None => {
                self.emit(Instruction::LocalSet(original));
                None
            }
        };
        let stored_local = stored.unwrap_or(original);
        match (constant, address) {
            (Some(constant), _) if constant < limit => {
                self.emit(Instruction::I32Const(constant as i32));
                self.emit(Instruction::LocalGet(stored_local));
                self.emit(store_instruction(view));
            }
            (Some(_), _) => {}
            (None, Some(address)) => {
                self.emit(Instruction::LocalGet(address));
                self.emit(Instruction::I32Const(limit as i32));
                self.emit(Instruction::I32LtU);
                self.emit(Instruction::If(BlockType::Empty));
                self.emit(Instruction::LocalGet(address));
                self.emit(Instruction::LocalGet(stored_local));
                self.emit(store_instruction(view));
                self.emit(Instruction::End);
                self.release(ValType::I32, address);
            }
            (None, None) => unreachable!("a dynamic address has a temp"),
        }
        if let Some(stored) = stored {
            self.release(view_val_type(view), stored);
        }
        self.emit(Instruction::LocalGet(original));
        self.release(value_val, original);
        self.module.memory = true;
        Ok(ty)
    }

    // -- 6.8.7 UnaryExpression ----------------------------------------------

    pub(super) fn unary(
        &mut self,
        operator: UnaryOperator,
        argument: &'a Expression<'a>,
    ) -> Result<AsmType> {
        match operator {
            UnaryOperator::UnaryNegation => {
                if let Some(value) = unsigned_literal(argument) {
                    return if value == 0 {
                        self.emit(Instruction::F64Const((-0.0f64).into()));
                        Ok(AsmType::DOUBLE)
                    } else if value <= 0x8000_0000 {
                        self.emit(Instruction::I32Const((value as i32).wrapping_neg()));
                        Ok(AsmType::SIGNED)
                    } else {
                        fail("integer literal out of range")
                    };
                }
                let ty = self.expression(argument)?;
                if ty.is_a(AsmType::INT) {
                    let value = self.temp(ValType::I32);
                    self.emit(Instruction::LocalSet(value));
                    self.emit(Instruction::I32Const(0));
                    self.emit(Instruction::LocalGet(value));
                    self.emit(Instruction::I32Sub);
                    self.release(ValType::I32, value);
                    self.additive_terms = 1;
                    Ok(AsmType::INTISH)
                } else if ty.is_a(AsmType::DOUBLEQ) {
                    self.emit(Instruction::F64Neg);
                    Ok(AsmType::DOUBLE)
                } else if ty.is_a(AsmType::FLOATQ) {
                    self.emit(Instruction::F32Neg);
                    Ok(AsmType::FLOATISH)
                } else {
                    fail("expected int, double? or float?")
                }
            }
            UnaryOperator::UnaryPlus => {
                let ty = match argument {
                    Expression::CallExpression(call) => {
                        self.call_expression(call, Coercion::Double)?
                    }
                    _ => self.expression(argument)?,
                };
                if ty.is_a(AsmType::SIGNED) {
                    self.emit(Instruction::F64ConvertI32S);
                } else if ty.is_a(AsmType::UNSIGNED) {
                    self.emit(Instruction::F64ConvertI32U);
                } else if ty.is_a(AsmType::DOUBLEQ) {
                } else if ty.is_a(AsmType::FLOATQ) {
                    self.emit(Instruction::F64PromoteF32);
                } else {
                    return fail("expected signed, unsigned, double? or float?");
                }
                Ok(AsmType::DOUBLE)
            }
            UnaryOperator::LogicalNot => {
                let ty = self.expression(argument)?;
                if !ty.is_a(AsmType::INT) {
                    return fail("expected int");
                }
                self.emit(Instruction::I32Eqz);
                Ok(ty)
            }
            UnaryOperator::BitwiseNot => {
                if let Expression::UnaryExpression(inner) = argument
                    && inner.operator == UnaryOperator::BitwiseNot
                {
                    let ty = self.expression(&inner.argument)?;
                    if ty.is_a(AsmType::DOUBLE) {
                        self.to_int32();
                    } else if ty.is_a(AsmType::FLOATQ) {
                        self.emit(Instruction::F64PromoteF32);
                        self.to_int32();
                    } else {
                        return fail("expected double or float?");
                    }
                    return Ok(AsmType::SIGNED);
                }
                let ty = self.expression(argument)?;
                if !ty.is_a(AsmType::INTISH) {
                    return fail("operator ~ expects intish");
                }
                self.emit(Instruction::I32Const(-1));
                self.emit(Instruction::I32Xor);
                Ok(AsmType::SIGNED)
            }
            _ => fail("unary operator outside asm.js"),
        }
    }

    /// `~~` of the double on the stack: JavaScript `ToInt32`.
    pub(super) fn to_int32(&mut self) {
        let value = self.temp(ValType::F64);
        self.emit(Instruction::LocalTee(value));
        self.emit(Instruction::F64Abs);
        self.emit(Instruction::F64Const(9_223_372_036_854_775_808.0f64.into()));
        self.emit(Instruction::F64Lt);
        self.emit(Instruction::If(BlockType::Result(ValType::I32)));
        self.emit(Instruction::LocalGet(value));
        self.emit(Instruction::I64TruncSatF64S);
        self.emit(Instruction::I32WrapI64);
        self.emit(Instruction::Else);
        self.emit(Instruction::LocalGet(value));
        self.call(Callee::Defined(TO_INT32_SLOW));
        self.emit(Instruction::End);
        self.release(ValType::F64, value);
    }

    // -- 6.8.8 - 6.8.15 Binary operators ------------------------------------

    pub(super) fn binary(
        &mut self,
        operator: BinaryOperator,
        left: &'a Expression<'a>,
        right: &'a Expression<'a>,
    ) -> Result<AsmType> {
        use BinaryOperator as B;
        match operator {
            B::Multiplication => self.multiply(left, right),
            B::Division | B::Remainder => {
                let a = self.expression(left)?;
                let b = self.expression(right)?;
                let division = operator == B::Division;
                if a.is_a(AsmType::DOUBLEQ) && b.is_a(AsmType::DOUBLEQ) {
                    if division {
                        self.emit(Instruction::F64Div);
                    } else {
                        let fmod = self.module.fmod_import();
                        self.call(Callee::Import(fmod));
                    }
                    Ok(AsmType::DOUBLE)
                } else if division && a.is_a(AsmType::FLOATQ) && b.is_a(AsmType::FLOATQ) {
                    self.emit(Instruction::F32Div);
                    Ok(AsmType::FLOATISH)
                } else if a.is_a(AsmType::SIGNED) && b.is_a(AsmType::SIGNED) {
                    self.integer_division(division, true);
                    Ok(AsmType::INTISH)
                } else if a.is_a(AsmType::UNSIGNED) && b.is_a(AsmType::UNSIGNED) {
                    self.integer_division(division, false);
                    Ok(AsmType::INTISH)
                } else {
                    fail("expected doubles, floats, signed or unsigned")
                }
            }
            B::Addition | B::Subtraction => self.additive(operator, left, right),
            B::ShiftLeft | B::ShiftRight | B::ShiftRightZeroFill => {
                self.intish_operands(left, right)?;
                Ok(match operator {
                    B::ShiftLeft => {
                        self.emit(Instruction::I32Shl);
                        AsmType::SIGNED
                    }
                    B::ShiftRight => {
                        self.emit(Instruction::I32ShrS);
                        AsmType::SIGNED
                    }
                    _ => {
                        self.emit(Instruction::I32ShrU);
                        AsmType::UNSIGNED
                    }
                })
            }
            B::LessThan
            | B::LessEqualThan
            | B::GreaterThan
            | B::GreaterEqualThan
            | B::Equality
            | B::Inequality => {
                let a = self.expression(left)?;
                let b = self.expression(right)?;
                let instruction = comparison(operator, a, b)?;
                self.emit(instruction);
                Ok(AsmType::INT)
            }
            B::BitwiseAnd | B::BitwiseXOR => {
                self.intish_operands(left, right)?;
                self.emit(if operator == B::BitwiseAnd {
                    Instruction::I32And
                } else {
                    Instruction::I32Xor
                });
                Ok(AsmType::SIGNED)
            }
            B::BitwiseOR => {
                if is_zero(right) {
                    // `x|0` coerces: a call takes a signed result, anything
                    // else must already be intish.
                    let ty = match left {
                        Expression::CallExpression(call) => {
                            self.call_expression(call, Coercion::Signed)?
                        }
                        _ => self.expression(left)?,
                    };
                    if !ty.is_a(AsmType::INTISH) {
                        return fail("expected intish for operator |");
                    }
                    return Ok(AsmType::SIGNED);
                }
                self.intish_operands(left, right)?;
                self.emit(Instruction::I32Or);
                Ok(AsmType::SIGNED)
            }
            _ => fail("binary operator outside asm.js"),
        }
    }

    pub(super) fn intish_operands(
        &mut self,
        left: &'a Expression<'a>,
        right: &'a Expression<'a>,
    ) -> Result<()> {
        let a = self.expression(left)?;
        let b = self.expression(right)?;
        if !(a.is_a(AsmType::INTISH) && b.is_a(AsmType::INTISH)) {
            return fail("expected intish operands");
        }
        Ok(())
    }

    /// `int * literal` and `literal * int` with |literal| < 2^20 multiply
    /// integers; every other product is of doubles or floats.
    pub(super) fn multiply(
        &mut self,
        left: &'a Expression<'a>,
        right: &'a Expression<'a>,
    ) -> Result<AsmType> {
        let small = |expression: &Expression<'_>| -> Option<Result<i32>> {
            match expression {
                Expression::NumericLiteral(_) => unsigned_literal(expression).map(|value| {
                    if value < 0x10_0000 {
                        Ok(value as i32)
                    } else {
                        fail("constant multiple out of range")
                    }
                }),
                Expression::UnaryExpression(unary)
                    if unary.operator == UnaryOperator::UnaryNegation =>
                {
                    match unsigned_literal(&unary.argument) {
                        Some(0) | None => None,
                        Some(value) if value < 0x10_0000 => Some(Ok(-(value as i32))),
                        Some(_) => Some(fail("constant multiple out of range")),
                    }
                }
                _ => None,
            }
        };
        // V8 takes a small leading literal as an integer factor only below
        // 2^20; a larger one is an ordinary operand.
        if let Some(Ok(factor)) = small(left) {
            self.emit(Instruction::I32Const(factor));
            self.expect(right, AsmType::INT)?;
            self.emit(Instruction::I32Mul);
            return Ok(AsmType::INTISH);
        }
        if let Some(factor) = small(right) {
            let factor = factor?;
            self.expect(left, AsmType::INT)?;
            self.emit(Instruction::I32Const(factor));
            self.emit(Instruction::I32Mul);
            return Ok(AsmType::INTISH);
        }
        let a = self.expression(left)?;
        let b = self.expression(right)?;
        if a.is_a(AsmType::DOUBLEQ) && b.is_a(AsmType::DOUBLEQ) {
            self.emit(Instruction::F64Mul);
            Ok(AsmType::DOUBLE)
        } else if a.is_a(AsmType::FLOATQ) && b.is_a(AsmType::FLOATQ) {
            self.emit(Instruction::F32Mul);
            Ok(AsmType::FLOATISH)
        } else {
            fail("expected doubles or floats")
        }
    }

    /// `+`/`-`: doubles, floats, or a chain of at most 2^20 `int` terms.
    pub(super) fn additive(
        &mut self,
        operator: BinaryOperator,
        left: &'a Expression<'a>,
        right: &'a Expression<'a>,
    ) -> Result<AsmType> {
        let a = self.expression(left)?;
        let left_terms = self.terms_of(left);
        let b = self.expression(right)?;
        let terms = left_terms.saturating_add(self.terms_of(right));
        let addition = operator == BinaryOperator::Addition;
        if a.is_a(AsmType::DOUBLE) && b.is_a(AsmType::DOUBLE) {
            self.emit(if addition {
                Instruction::F64Add
            } else {
                Instruction::F64Sub
            });
            Ok(AsmType::DOUBLE)
        } else if a.is_a(AsmType::FLOATQ) && b.is_a(AsmType::FLOATQ) {
            self.emit(if addition {
                Instruction::F32Add
            } else {
                Instruction::F32Sub
            });
            Ok(AsmType::FLOATISH)
        } else if additive_int(a, left) && additive_int(b, right) {
            if terms > 1 << 20 {
                return fail("more than 2^20 additive values");
            }
            self.additive_terms = terms;
            self.emit(if addition {
                Instruction::I32Add
            } else {
                Instruction::I32Sub
            });
            Ok(AsmType::INTISH)
        } else {
            fail("illegal types for + or -")
        }
    }

    /// `int` terms an already evaluated additive operand contributes.
    pub(super) fn terms_of(&self, operand: &Expression<'_>) -> u32 {
        if additive_chain(operand) {
            self.additive_terms
        } else {
            1
        }
    }

    /// Integer `/` or `%` of the two operands on the stack, total as
    /// JavaScript's `(a / b) | 0` and `(a % b) | 0` are.
    pub(super) fn integer_division(&mut self, division: bool, signed: bool) {
        let b = self.temp(ValType::I32);
        let a = self.temp(ValType::I32);
        self.emit(Instruction::LocalSet(b));
        self.emit(Instruction::LocalSet(a));
        self.emit(Instruction::LocalGet(b));
        self.emit(Instruction::I32Eqz);
        self.emit(Instruction::If(BlockType::Result(ValType::I32)));
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::Else);
        if division && signed {
            // INT_MIN / -1 overflows to INT_MIN, as `| 0` wraps 2^31.
            self.emit(Instruction::LocalGet(b));
            self.emit(Instruction::I32Const(-1));
            self.emit(Instruction::I32Eq);
            self.emit(Instruction::If(BlockType::Result(ValType::I32)));
            self.emit(Instruction::I32Const(0));
            self.emit(Instruction::LocalGet(a));
            self.emit(Instruction::I32Sub);
            self.emit(Instruction::Else);
            self.emit(Instruction::LocalGet(a));
            self.emit(Instruction::LocalGet(b));
            self.emit(Instruction::I32DivS);
            self.emit(Instruction::End);
        } else {
            self.emit(Instruction::LocalGet(a));
            self.emit(Instruction::LocalGet(b));
            self.emit(match (division, signed) {
                (true, false) => Instruction::I32DivU,
                (false, true) => Instruction::I32RemS,
                _ => Instruction::I32RemU,
            });
        }
        self.emit(Instruction::End);
        self.release(ValType::I32, a);
        self.release(ValType::I32, b);
    }

    // -- 6.9 ValidateCall ---------------------------------------------------

    pub(super) fn call_expression(
        &mut self,
        call: &'a CallExpression<'a>,
        coercion: Coercion,
    ) -> Result<AsmType> {
        if call.optional {
            return fail("optional call");
        }
        match &call.callee {
            Expression::Identifier(id) if !self.locals.contains_key(id.name.as_str()) => {
                match self.module.names.get(id.name.as_str()).copied() {
                    Some(Binding::Stdlib(StdlibFunction::Fround)) => self.float_coercion(call),
                    Some(Binding::Stdlib(function)) => self.stdlib_call(function, call, coercion),
                    Some(Binding::Foreign(foreign)) => self.foreign_call(foreign, call, coercion),
                    Some(Binding::Function(function)) => self.direct_call(function, call, coercion),
                    _ => fail("call target is not a function"),
                }
            }
            Expression::ComputedMemberExpression(member) => self.table_call(member, call, coercion),
            _ => fail("call target is not a function"),
        }
    }

    /// Evaluate the arguments: `(specific types, generalized parameter types)`.
    pub(super) fn arguments(
        &mut self,
        call: &'a CallExpression<'a>,
    ) -> Result<(Vec<AsmType>, Vec<AsmType>)> {
        let mut specific = Vec::with_capacity(call.arguments.len());
        let mut general = Vec::with_capacity(call.arguments.len());
        for argument in &call.arguments {
            let Some(argument) = argument.as_expression() else {
                return fail("spread argument");
            };
            let ty = self.expression(argument)?;
            specific.push(ty);
            general.push(if ty.is_a(AsmType::INT) {
                AsmType::INT
            } else if ty.is_a(AsmType::FLOAT) {
                AsmType::FLOAT
            } else if ty.is_a(AsmType::DOUBLE) {
                AsmType::DOUBLE
            } else {
                return fail("bad function argument type");
            });
        }
        Ok((specific, general))
    }

    pub(super) fn direct_call(
        &mut self,
        function: u32,
        call: &'a CallExpression<'a>,
        coercion: Coercion,
    ) -> Result<AsmType> {
        let (specific, general) = self.arguments(call)?;
        let result = coercion.result();
        let info = &mut self.module.functions[function as usize];
        match &info.signature {
            None => {
                info.signature = Some(Signature {
                    params: general,
                    result,
                })
            }
            Some(signature) if signature.accepts(result, &specific) => {}
            Some(_) => return fail("function use doesn't match definition"),
        }
        self.call(Callee::Defined(HELPER_COUNT + function));
        Ok(result)
    }

    pub(super) fn table_call(
        &mut self,
        member: &'a ComputedMemberExpression<'a>,
        call: &'a CallExpression<'a>,
        coercion: Coercion,
    ) -> Result<AsmType> {
        let Expression::Identifier(id) = &member.object else {
            return fail("expected function table");
        };
        if member.optional || self.locals.contains_key(id.name.as_str()) {
            return fail("expected function table");
        }
        let Some(Binding::Table(table)) = self.module.names.get(id.name.as_str()).copied() else {
            return fail("expected function table");
        };
        let Expression::BinaryExpression(index) = &member.expression else {
            return fail("expected masked table index");
        };
        let (mask, base) = {
            let info = &self.module.tables[table as usize];
            (info.mask, info.base)
        };
        if index.operator != BinaryOperator::BitwiseAnd
            || unsigned_literal(&index.right) != Some(mask)
        {
            return fail("table mask mismatch");
        }
        if !self.expression(&index.left)?.is_a(AsmType::INTISH) {
            return fail("expected intish index");
        }
        self.emit(Instruction::I32Const(mask as i32));
        self.emit(Instruction::I32And);
        if base != 0 {
            self.emit(Instruction::I32Const(base as i32));
            self.emit(Instruction::I32Add);
        }
        let slot = self.temp(ValType::I32);
        self.emit(Instruction::LocalSet(slot));
        let (specific, general) = self.arguments(call)?;
        let result = coercion.result();
        let signature = Signature {
            params: general,
            result,
        };
        let info = &mut self.module.tables[table as usize];
        match &info.signature {
            None => info.signature = Some(signature.clone()),
            Some(existing) if existing.accepts(result, &specific) => {}
            Some(_) => return fail("function table use doesn't match definition"),
        }
        let type_index = self.module.signature_type(&signature);
        self.emit(Instruction::LocalGet(slot));
        self.release(ValType::I32, slot);
        self.emit(Instruction::CallIndirect {
            type_index,
            table_index: 0,
        });
        Ok(result)
    }

    pub(super) fn foreign_call(
        &mut self,
        foreign: u32,
        call: &'a CallExpression<'a>,
        coercion: Coercion,
    ) -> Result<AsmType> {
        let (specific, _) = self.arguments(call)?;
        let mut params = Vec::with_capacity(specific.len());
        for ty in &specific {
            if !ty.is_a(AsmType::EXTERN) {
                return fail("imported function args must be extern");
            }
            params.push(if ty.is_a(AsmType::DOUBLE) {
                ValType::F64
            } else {
                ValType::I32
            });
        }
        let result = match coercion {
            Coercion::None => None,
            Coercion::Signed => Some(ValType::I32),
            Coercion::Double => Some(ValType::F64),
            Coercion::Float => return fail("imported function can't be called as float"),
        };
        let import = self.module.foreign_import(foreign, params, result);
        self.call(Callee::Import(import));
        Ok(coercion.result())
    }

    /// `fround(x)` (asm.js §6.11 ValidateFloatCoercion).
    pub(super) fn float_coercion(&mut self, call: &'a CallExpression<'a>) -> Result<AsmType> {
        let [argument] = call.arguments.as_slice() else {
            return fail("fround takes one argument");
        };
        let Some(argument) = argument.as_expression() else {
            return fail("spread argument");
        };
        let ty = match argument {
            Expression::CallExpression(inner) => self.call_expression(inner, Coercion::Float)?,
            _ => self.expression(argument)?,
        };
        if ty.is_a(AsmType::FLOATISH) {
        } else if ty.is_a(AsmType::DOUBLEQ) {
            self.emit(Instruction::F32DemoteF64);
        } else if ty.is_a(AsmType::SIGNED) {
            self.emit(Instruction::F32ConvertI32S);
        } else if ty.is_a(AsmType::UNSIGNED) {
            self.emit(Instruction::F32ConvertI32U);
        } else {
            return fail("illegal conversion to float");
        }
        Ok(AsmType::FLOAT)
    }

    pub(super) fn stdlib_call(
        &mut self,
        function: StdlibFunction,
        call: &'a CallExpression<'a>,
        coercion: Coercion,
    ) -> Result<AsmType> {
        use StdlibFunction as F;
        let (args, _) = self.arguments(call)?;
        let requested = coercion.result();
        // V8 tries the context's result first, then float, floatish, double,
        // signed and unsigned, against the member's overloads.
        let candidates = [
            requested,
            AsmType::FLOAT,
            AsmType::FLOATISH,
            AsmType::DOUBLE,
            AsmType::SIGNED,
            AsmType::UNSIGNED,
        ];
        let accepts = |result: AsmType| stdlib_overload(function, result, &args);
        let Some(result) = candidates.into_iter().find(|result| accepts(*result)) else {
            return fail("stdlib use doesn't match its type");
        };
        let first = args[0];
        match function {
            F::Acos | F::Asin | F::Atan | F::Cos | F::Sin | F::Tan | F::Exp | F::Log => {
                let import = self.module.math_import(function, math_name(function), 1);
                self.call(Callee::Import(import));
            }
            F::Atan2 | F::Pow => {
                let import = self.module.math_import(function, math_name(function), 2);
                self.call(Callee::Import(import));
            }
            F::Imul => self.emit(Instruction::I32Mul),
            F::Clz32 => self.emit(Instruction::I32Clz),
            F::Ceil | F::Floor | F::Sqrt => {
                let double = first.is_a(AsmType::DOUBLEQ);
                self.emit(match (function, double) {
                    (F::Ceil, true) => Instruction::F64Ceil,
                    (F::Ceil, false) => Instruction::F32Ceil,
                    (F::Floor, true) => Instruction::F64Floor,
                    (F::Floor, false) => Instruction::F32Floor,
                    (_, true) => Instruction::F64Sqrt,
                    (_, false) => Instruction::F32Sqrt,
                });
            }
            F::Abs => {
                if first.is_a(AsmType::SIGNED) {
                    // (x ^ (x >> 31)) - (x >> 31)
                    let x = self.temp(ValType::I32);
                    self.emit(Instruction::LocalTee(x));
                    self.emit(Instruction::LocalGet(x));
                    self.emit(Instruction::I32Const(31));
                    self.emit(Instruction::I32ShrS);
                    self.emit(Instruction::LocalTee(x));
                    self.emit(Instruction::I32Xor);
                    self.emit(Instruction::LocalGet(x));
                    self.emit(Instruction::I32Sub);
                    self.release(ValType::I32, x);
                } else if first.is_a(AsmType::DOUBLEQ) {
                    self.emit(Instruction::F64Abs);
                } else {
                    self.emit(Instruction::F32Abs);
                }
            }
            F::Min | F::Max => {
                let min = function == F::Min;
                if first.is_a(AsmType::DOUBLE) {
                    for _ in 1..args.len() {
                        self.emit(if min {
                            Instruction::F64Min
                        } else {
                            Instruction::F64Max
                        });
                    }
                } else if first.is_a(AsmType::FLOAT) {
                    for _ in 1..args.len() {
                        self.emit(if min {
                            Instruction::F32Min
                        } else {
                            Instruction::F32Max
                        });
                    }
                } else {
                    // Fold left: acc = (acc >= x) == min ? x : acc.
                    let x = self.temp(ValType::I32);
                    let acc = self.temp(ValType::I32);
                    for _ in 1..args.len() {
                        self.emit(Instruction::LocalSet(x));
                        self.emit(Instruction::LocalSet(acc));
                        self.emit(Instruction::LocalGet(x));
                        self.emit(Instruction::LocalGet(acc));
                        self.emit(Instruction::LocalGet(acc));
                        self.emit(Instruction::LocalGet(x));
                        self.emit(if min {
                            Instruction::I32GeS
                        } else {
                            Instruction::I32LeS
                        });
                        self.emit(Instruction::Select);
                    }
                    self.release(ValType::I32, acc);
                    self.release(ValType::I32, x);
                }
            }
            _ => return fail("not a callable stdlib member"),
        }
        Ok(result)
    }
}
