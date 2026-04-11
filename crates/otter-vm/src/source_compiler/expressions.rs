//! Expression compilation: literals, identifiers, `this`, unary/binary/logical
//! operators, object and array literals, plain member access, delete, conditional,
//! sequence, yield/await, optional chain, template + tagged template, BigInt,
//! regexp, dynamic `import()`, and `import.meta`. Calls, super-calls, `new`, and
//! update/member-update operators live in `calls.rs`; function/arrow/class
//! expressions live in `functions_and_classes.rs`.

use super::ast::non_computed_property_key_name;
use super::module_compiler::ModuleCompiler;
use super::shared::{Binding, FunctionCompiler, FunctionKind, ValueLocation};
use super::*;

impl<'a> FunctionCompiler<'a> {
    pub(super) fn compile_expression_with_inferred_name(
        &mut self,
        expression: &Expression<'_>,
        inferred_name: Option<&str>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match expression {
            Expression::FunctionExpression(function) => {
                self.compile_function_expression(function, inferred_name, module)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                self.compile_arrow_function_expression(arrow, inferred_name, module)
            }
            // §13.2.5.5 NamedEvaluation — when an anonymous class expression
            // appears in a binding/assignment target, the class should take
            // the contextual name instead of the default "anonymous"
            // placeholder. Fall through to the normal path when the class is
            // already named.
            Expression::ClassExpression(class) if class.id.is_none() => {
                self.compile_class_expression_with_name(class, inferred_name, module)
            }
            _ => self.compile_expression(expression, module),
        }
    }

    pub(super) fn compile_expression(
        &mut self,
        expression: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // Span attribution lives at the *statement* level — the enclosing
        // `compile_statement` call records the active source location once
        // and we keep that location until the next statement. Sub-expressions
        // intentionally do NOT re-record because that would clobber the
        // outer statement span and make the diagnostic underline land in the
        // middle of the throw argument (e.g., `b.value` inside
        // `throw new TypeError(b.value as string)`) instead of the `throw`
        // keyword itself.
        match expression {
            Expression::NumericLiteral(literal) => {
                // §B.1.1 — Legacy octal literals are a SyntaxError in strict mode.
                // Spec: <https://tc39.es/ecma262/#sec-additional-syntax-numeric-literals>
                if self.strict_mode
                    && let Some(raw) = literal.raw.as_ref()
                    && is_legacy_octal_literal(raw.as_str())
                {
                    return Err(SourceLoweringError::Parse(format!(
                        "Octal literals are not allowed in strict mode: {}",
                        raw.as_str()
                    )));
                }
                self.compile_numeric_literal(literal.value)
            }
            Expression::BooleanLiteral(literal) => self.compile_bool(literal.value),
            Expression::NullLiteral(_) => self.load_null(),
            Expression::StringLiteral(literal) => {
                // §B.1.2 — Legacy octal escape sequences are a SyntaxError in strict mode.
                // Spec: <https://tc39.es/ecma262/#sec-additional-syntax-string-literals>
                if self.strict_mode
                    && let Some(raw) = literal.raw.as_ref()
                    && let Some(escape) = find_legacy_octal_escape(raw.as_str())
                {
                    return Err(SourceLoweringError::Parse(format!(
                        "Octal escape sequences are not allowed in strict mode: {escape}"
                    )));
                }
                if literal.lone_surrogates {
                    self.compile_js_string_literal(crate::js_string::JsString::from_oxc_encoded(
                        literal.value.as_str(),
                    ))
                } else {
                    self.compile_string_literal(literal.value.as_str())
                }
            }
            Expression::ThisExpression(_) => self.compile_this_expression(),
            Expression::ArrayExpression(array) => self.compile_array_expression(array, module),
            Expression::Identifier(identifier) => self.compile_identifier(identifier.name.as_str()),
            Expression::ParenthesizedExpression(parenthesized) => {
                self.compile_expression(&parenthesized.expression, module)
            }
            Expression::AssignmentExpression(assignment) => {
                self.compile_assignment_expression(assignment, module)
            }
            Expression::BinaryExpression(binary) => self.compile_binary_expression(binary, module),
            Expression::LogicalExpression(logical) => {
                self.compile_logical_expression(logical, module)
            }
            Expression::UnaryExpression(unary) => self.compile_unary_expression(unary, module),
            Expression::UpdateExpression(update) => self.compile_update_expression(update, module),
            Expression::CallExpression(call) => self.compile_call_expression(call, module),
            Expression::NewExpression(new_expression) => {
                self.compile_new_expression(new_expression, module)
            }
            Expression::FunctionExpression(function) => {
                self.compile_function_expression(function, None, module)
            }
            Expression::ObjectExpression(object) => self.compile_object_expression(object, module),
            Expression::StaticMemberExpression(member) => {
                self.compile_static_member_expression(member, module)
            }
            Expression::ComputedMemberExpression(member) => {
                self.compile_computed_member_expression(member, module)
            }
            Expression::ConditionalExpression(conditional) => {
                self.compile_conditional_expression(conditional, module)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                self.compile_arrow_function_expression(arrow, None, module)
            }
            Expression::TemplateLiteral(template) => {
                self.compile_template_literal(template, module)
            }
            // §13.3.11 Tagged Templates — `tag`str``
            // Spec: <https://tc39.es/ecma262/#sec-tagged-templates>
            Expression::TaggedTemplateExpression(tagged) => {
                self.compile_tagged_template_expression(tagged, module)
            }
            Expression::SequenceExpression(sequence) => {
                self.compile_sequence_expression(sequence, module)
            }
            // §15.7 ClassExpression
            // Spec: <https://tc39.es/ecma262/#sec-class-definitions-runtime-semantics-evaluation>
            Expression::ClassExpression(class) => self.compile_class_expression(class, module),
            // §13.3.7 Optional Chaining (`?.`)
            // Spec: <https://tc39.es/ecma262/#sec-optional-chaining>
            Expression::ChainExpression(chain) => self.compile_chain_expression(chain, module),
            // §14.4 Yield — `yield expr` / `yield`
            // Spec: <https://tc39.es/ecma262/#sec-yield>
            Expression::YieldExpression(yield_expr) => {
                self.compile_yield_expression(yield_expr, module)
            }
            // §14.7 Await — `await expr`
            // Spec: <https://tc39.es/ecma262/#sec-await>
            Expression::AwaitExpression(await_expr) => {
                self.compile_await_expression(await_expr, module)
            }
            // §12.8.6 BigInt Literals
            // Spec: <https://tc39.es/ecma262/#sec-numeric-literals>
            Expression::BigIntLiteral(lit) => {
                let raw = lit.raw.as_ref().map_or("0n", |a| a.as_str());
                self.compile_bigint_literal(raw)
            }
            // §12.9 Regular Expression Literals
            // Spec: <https://tc39.es/ecma262/#sec-literals-regular-expression-literals>
            Expression::RegExpLiteral(regexp) => {
                let pattern = regexp.regex.pattern.text.as_str();
                let flags = regexp.regex.flags.to_string();
                self.compile_regexp_literal(pattern, &flags)
            }
            // §13.10 Private Field Access — `obj.#field`
            // Spec: <https://tc39.es/ecma262/#sec-private-field-access>
            Expression::PrivateFieldExpression(member) => {
                self.compile_private_field_get(member, module)
            }
            // §13.10.1 PrivateInExpression — `#field in obj`
            // Spec: <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
            Expression::PrivateInExpression(expr) => {
                self.compile_private_in_expression(expr, module)
            }
            // §13.3.10 Dynamic import() — `import(specifier)`.
            // Spec: <https://tc39.es/ecma262/#sec-import-calls>
            Expression::ImportExpression(import_expr) => {
                self.compile_import_expression(import_expr, module)
            }
            // §13.3.12 MetaProperty — `import.meta` or `new.target`.
            // Spec: <https://tc39.es/ecma262/#sec-meta-properties>
            Expression::MetaProperty(meta) => self.compile_meta_property(meta),
            _ => Err(SourceLoweringError::Unsupported(format!(
                "expression {:?}",
                expression
            ))),
        }
    }

    pub(super) fn compile_numeric_literal(
        &mut self,
        value: f64,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // NaN is handled by a dedicated opcode.
        if value.is_nan() {
            let register = self.alloc_temp();
            self.instructions.push(Instruction::load_nan(register));
            return Ok(ValueLocation::temp(register));
        }
        // Integers that fit in i32 use the compact LoadI32 encoding.
        if value.is_finite()
            && value.fract() == 0.0
            && value >= i32::MIN as f64
            && value <= i32::MAX as f64
        {
            return self.load_i32(value as i32);
        }
        // General float64 values go through the float constant table.
        self.load_f64(value)
    }

    fn load_f64(&mut self, value: f64) -> Result<ValueLocation, SourceLoweringError> {
        let id = if let Some(pos) = self
            .float_constants
            .iter()
            .position(|v| v.to_bits() == value.to_bits())
        {
            FloatId(pos as u16)
        } else {
            let id = FloatId(self.float_constants.len() as u16);
            self.float_constants.push(value);
            id
        };
        let register = self.alloc_temp();
        self.instructions.push(Instruction::load_f64(register, id));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn compile_string_literal(
        &mut self,
        value: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        let string_id = self.intern_string(value)?;
        self.instructions
            .push(Instruction::load_string(register, string_id));
        Ok(ValueLocation::temp(register))
    }

    /// Compiles a string literal with lone surrogates (WTF-16) from oxc.
    pub(super) fn compile_js_string_literal(
        &mut self,
        value: crate::js_string::JsString,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let register = self.alloc_temp();
        let string_id = self.intern_js_string(value)?;
        self.instructions
            .push(Instruction::load_string(register, string_id));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn compile_this_expression(&mut self) -> Result<ValueLocation, SourceLoweringError> {
        // Arrow functions capture `this` lexically — resolve via the "this" binding.
        // Derived constructors also use the local "this" binding so that the
        // value is upvalue-capturable by nested arrows. This ensures that
        // `() => super()` inside a derived ctor can write `this` back through
        // the shared UpvalueCell and the constructor body sees it.
        // Spec: <https://tc39.es/ecma262/#sec-getthisenvironment>
        if self.kind == FunctionKind::Arrow
            || self.kind == FunctionKind::AsyncArrow
            || self.is_derived_constructor
        {
            return self.compile_identifier("this");
        }
        let register = self.alloc_temp();
        self.instructions.push(Instruction::load_this(register));
        Ok(ValueLocation::temp(register))
    }

    pub(super) fn compile_identifier(
        &mut self,
        name: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // Global value identifiers — always available, not bindings.
        if !self.scope.borrow().bindings.contains_key(name) {
            match name {
                "undefined" => return self.load_undefined(),
                "NaN" => return self.load_nan(),
                "Infinity" => return self.load_f64(f64::INFINITY),
                _ => {}
            }
        }

        match self.resolve_binding(name) {
            Ok(Binding::Register(register)) => {
                if self.parameter_tdz_active {
                    self.emit_assert_not_hole(register);
                }
                Ok(ValueLocation::local(register))
            }
            Ok(Binding::ThisRegister(register)) => {
                self.emit_assert_not_hole(register);
                Ok(ValueLocation::local(register))
            }
            Ok(Binding::Function {
                closure_register, ..
            }) => Ok(ValueLocation::local(closure_register)),
            Ok(Binding::Upvalue(upvalue)) => {
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::get_upvalue(register, upvalue));
                Ok(ValueLocation::temp(register))
            }
            Ok(Binding::ThisUpvalue(upvalue)) | Ok(Binding::ImmutableUpvalue(upvalue)) => {
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::get_upvalue(register, upvalue));
                self.emit_assert_not_hole(register);
                Ok(ValueLocation::temp(register))
            }
            Ok(Binding::ImmutableRegister(register)) => {
                self.emit_assert_not_hole(register);
                Ok(ValueLocation::local(register))
            }
            Err(SourceLoweringError::UnknownBinding(_)) => {
                // Undeclared variable → runtime global lookup (V8's LdaGlobal).
                let property = self.intern_property_name(name)?;
                let register = self.alloc_temp();
                self.instructions
                    .push(Instruction::get_global(register, property));
                Ok(ValueLocation::temp(register))
            }
            Err(e) => Err(e),
        }
    }

    fn compile_binary_expression(
        &mut self,
        binary: &oxc_ast::ast::BinaryExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let lhs = self.compile_expression(&binary.left, module)?;
        let lhs = self.stabilize_binding_value(lhs)?;
        let rhs = self.compile_expression(&binary.right, module)?;

        let result = if lhs.is_temp {
            lhs
        } else if rhs.is_temp {
            rhs
        } else {
            ValueLocation::temp(self.alloc_temp())
        };

        #[allow(unreachable_patterns)]
        match binary.operator {
            BinaryOperator::Addition => {
                self.instructions.push(Instruction::add(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Subtraction => {
                self.instructions.push(Instruction::sub(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Multiplication => {
                self.instructions.push(Instruction::mul(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Division => {
                self.instructions.push(Instruction::div(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::LessThan => {
                self.instructions.push(Instruction::lt(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::GreaterThan => {
                self.instructions.push(Instruction::gt(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::GreaterEqualThan => {
                self.instructions.push(Instruction::gte(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::LessEqualThan => {
                self.instructions.push(Instruction::lte(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Remainder => {
                self.instructions.push(Instruction::mod_(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Equality => {
                self.instructions.push(Instruction::loose_eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::StrictEquality => {
                self.instructions.push(Instruction::eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Inequality => {
                self.instructions.push(Instruction::loose_eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
                self.instructions
                    .push(Instruction::not(result.register, result.register));
            }
            BinaryOperator::StrictInequality => {
                self.instructions.push(Instruction::eq(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
                self.instructions
                    .push(Instruction::not(result.register, result.register));
            }
            BinaryOperator::Instanceof => {
                self.instructions.push(Instruction::instance_of(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::In => {
                self.instructions.push(Instruction::has_property(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::BitwiseAnd => {
                self.instructions.push(Instruction::bit_and(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::BitwiseOR => {
                self.instructions.push(Instruction::bit_or(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::BitwiseXOR => {
                self.instructions.push(Instruction::bit_xor(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::ShiftLeft => {
                self.instructions.push(Instruction::shl(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::ShiftRight => {
                self.instructions.push(Instruction::shr(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::ShiftRightZeroFill => {
                self.instructions.push(Instruction::ushr(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            BinaryOperator::Exponential => {
                self.instructions.push(Instruction::exp(
                    result.register,
                    lhs.register,
                    rhs.register,
                ));
            }
            _ => {
                return Err(SourceLoweringError::Unsupported(format!(
                    "binary operator {:?}",
                    binary.operator
                )));
            }
        }

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

    fn compile_logical_expression(
        &mut self,
        logical: &oxc_ast::ast::LogicalExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let left = self.compile_expression(&logical.left, module)?;
        let result = if left.is_temp {
            left
        } else {
            let register = self.alloc_temp();
            self.instructions
                .push(Instruction::move_(register, left.register));
            ValueLocation::temp(register)
        };

        let short_circuit = match logical.operator {
            LogicalOperator::And => {
                self.emit_conditional_placeholder(Opcode::JumpIfFalse, result.register)
            }
            LogicalOperator::Or => {
                self.emit_conditional_placeholder(Opcode::JumpIfTrue, result.register)
            }
            LogicalOperator::Coalesce => {
                // ?? : short-circuit if LHS is not null/undefined.
                let null_val = self.load_null()?;
                let cmp = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    result.register,
                    null_val.register,
                ));
                self.release(null_val);
                let jump_if_null =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, cmp.register);

                let undef_val = self.load_undefined()?;
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    result.register,
                    undef_val.register,
                ));
                self.release(undef_val);
                let jump_if_undef =
                    self.emit_conditional_placeholder(Opcode::JumpIfTrue, cmp.register);
                self.release(cmp);

                // Not nullish — skip RHS.
                let skip_rhs = self.emit_jump_placeholder();

                let rhs_start = self.instructions.len();
                self.patch_jump(jump_if_null, rhs_start)?;
                self.patch_jump(jump_if_undef, rhs_start)?;

                let right = self.compile_expression(&logical.right, module)?;
                if right.register != result.register {
                    self.instructions
                        .push(Instruction::move_(result.register, right.register));
                    self.release(right);
                }

                self.patch_jump(skip_rhs, self.instructions.len())?;
                return Ok(result);
            }
        };

        let right = self.compile_expression(&logical.right, module)?;
        if right.register != result.register {
            self.instructions
                .push(Instruction::move_(result.register, right.register));
            self.release(right);
        }
        self.patch_jump(short_circuit, self.instructions.len())?;
        Ok(result)
    }

    fn compile_unary_expression(
        &mut self,
        unary: &oxc_ast::ast::UnaryExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        match unary.operator {
            UnaryOperator::UnaryNegation => {
                // §6.1.6.2.1 BigInt::unaryMinus — compile `-42n` as BigInt("-42").
                // Normalize the literal source first so non-decimal bases
                // (`-0xFFn`, `-0b1_0n`) collapse to the canonical decimal form
                // expected by the runtime's BigInt storage invariant.
                if let Expression::BigIntLiteral(lit) = &unary.argument {
                    let raw = lit.raw.as_ref().map_or("0n", |a| a.as_str());
                    let decimal = normalize_bigint_literal(raw).map_err(|err| {
                        SourceLoweringError::Unsupported(format!(
                            "invalid BigInt literal {raw}: {err}"
                        ))
                    })?;
                    let negated = if decimal == "0" {
                        // §6.1.6.2 BigInt is a mathematical integer — no `-0n`.
                        "0".to_string()
                    } else if let Some(rest) = decimal.strip_prefix('-') {
                        rest.to_string()
                    } else {
                        format!("-{decimal}")
                    };
                    return self.compile_bigint_literal_value(&negated);
                }
                let zero = self.load_i32(0)?;
                let argument = self.compile_expression(&unary.argument, module)?;
                // Use `zero` as the result register since it was allocated first —
                // the stack-based temp allocator requires LIFO release order.
                // Putting the result in `zero` (lower on the stack) and releasing
                // `argument` (higher) preserves the invariant.
                self.instructions.push(Instruction::sub(
                    zero.register,
                    zero.register,
                    argument.register,
                ));
                if argument.is_temp {
                    self.release(argument);
                }
                Ok(zero)
            }
            UnaryOperator::UnaryPlus => {
                let argument = self.compile_expression(&unary.argument, module)?;
                let result = if argument.is_temp {
                    argument
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions
                    .push(Instruction::to_number(result.register, argument.register));
                if result.register != argument.register {
                    self.release(argument);
                }
                Ok(result)
            }
            UnaryOperator::Typeof => {
                // ES2024 §13.5.1: typeof on an unresolvable global reference
                // must return "undefined", not throw ReferenceError.
                if let oxc_ast::ast::Expression::Identifier(ident) = &unary.argument
                    && !self
                        .scope
                        .borrow()
                        .bindings
                        .contains_key(ident.name.as_str())
                {
                    // Global variable — use TypeOfGlobal which doesn't throw.
                    let result = ValueLocation::temp(self.alloc_temp());
                    let prop = self.intern_property_name(ident.name.as_str())?;
                    self.instructions
                        .push(Instruction::type_of_global(result.register, prop));
                    return Ok(result);
                }
                let value = self.compile_expression(&unary.argument, module)?;
                let result = if value.is_temp {
                    value
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions
                    .push(Instruction::type_of(result.register, value.register));
                Ok(result)
            }
            UnaryOperator::LogicalNot => {
                let value = self.compile_expression(&unary.argument, module)?;
                let result = if value.is_temp {
                    value
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions
                    .push(Instruction::not(result.register, value.register));
                Ok(result)
            }
            UnaryOperator::BitwiseNot => {
                let value = self.compile_expression(&unary.argument, module)?;
                let value = self.materialize_value(value);
                let minus_one = self.load_i32(-1)?;
                let result = if value.is_temp {
                    value
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                // ~x === x ^ -1
                self.instructions.push(Instruction::bit_xor(
                    result.register,
                    value.register,
                    minus_one.register,
                ));
                self.release(minus_one);
                if result.register != value.register {
                    self.release(value);
                }
                Ok(result)
            }
            UnaryOperator::Void => {
                let value = self.compile_expression(&unary.argument, module)?;
                self.release(value);
                self.load_undefined()
            }
            UnaryOperator::Delete => self.compile_delete_expression(&unary.argument, module),
        }
    }

    fn compile_object_expression(
        &mut self,
        object: &oxc_ast::ast::ObjectExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let destination = ValueLocation::local(self.allocate_local()?);
        self.instructions
            .push(Instruction::new_object(destination.register));

        for property in &object.properties {
            let ObjectPropertyKind::ObjectProperty(property) = property else {
                let ObjectPropertyKind::SpreadProperty(spread) = property else {
                    unreachable!("object literal property kind should be exhaustively handled");
                };
                let source = self.compile_expression(&spread.argument, module)?;
                self.instructions.push(Instruction::copy_data_properties(
                    destination.register,
                    source.register,
                ));
                self.release(source);
                continue;
            };
            if property.kind != PropertyKind::Init
                && !matches!(property.kind, PropertyKind::Get | PropertyKind::Set)
            {
                return Err(SourceLoweringError::Unsupported(
                    "object getters/setters/methods".to_string(),
                ));
            }
            if matches!(property.kind, PropertyKind::Get | PropertyKind::Set) {
                let inferred_name = if property.computed {
                    None
                } else {
                    let name = non_computed_property_key_name(&property.key).ok_or_else(|| {
                        SourceLoweringError::Unsupported("object accessor property key".to_string())
                    })?;
                    Some(match property.kind {
                        PropertyKind::Get => format!("get {name}"),
                        PropertyKind::Set => format!("set {name}"),
                        _ => unreachable!(),
                    })
                };
                let accessor = self.compile_expression_with_inferred_name(
                    &property.value,
                    inferred_name.as_deref(),
                    module,
                )?;
                if property.computed {
                    let key = self.compile_expression(property.key.to_expression(), module)?;
                    match property.kind {
                        PropertyKind::Get => {
                            self.instructions.push(Instruction::define_computed_getter(
                                destination.register,
                                key.register,
                                accessor.register,
                            ))
                        }
                        PropertyKind::Set => {
                            self.instructions.push(Instruction::define_computed_setter(
                                destination.register,
                                key.register,
                                accessor.register,
                            ))
                        }
                        _ => unreachable!(),
                    }
                    self.release(key);
                } else {
                    let name = non_computed_property_key_name(&property.key).ok_or_else(|| {
                        SourceLoweringError::Unsupported("object accessor property key".to_string())
                    })?;
                    let property_id = self.intern_property_name(&name)?;
                    match property.kind {
                        PropertyKind::Get => {
                            self.instructions.push(Instruction::define_named_getter(
                                destination.register,
                                accessor.register,
                                property_id,
                            ))
                        }
                        PropertyKind::Set => {
                            self.instructions.push(Instruction::define_named_setter(
                                destination.register,
                                accessor.register,
                                property_id,
                            ))
                        }
                        _ => unreachable!(),
                    }
                }
                self.release(accessor);
                continue;
            }
            if property.computed {
                let key = self.compile_expression(property.key.to_expression(), module)?;
                let key = if key.is_temp {
                    self.stabilize_binding_value(key)?
                } else {
                    key
                };
                let value = self.compile_expression(&property.value, module)?;
                self.instructions.push(Instruction::set_index(
                    destination.register,
                    key.register,
                    value.register,
                ));
                self.release(key);
                self.release(value);
            } else {
                let name = non_computed_property_key_name(&property.key).ok_or_else(|| {
                    SourceLoweringError::Unsupported("object property key".to_string())
                })?;
                let property_id = self.intern_property_name(&name)?;
                let value = self.compile_expression_with_inferred_name(
                    &property.value,
                    Some(&name),
                    module,
                )?;
                self.instructions.push(Instruction::set_property(
                    destination.register,
                    value.register,
                    property_id,
                ));
                self.release(value);
            }
        }

        Ok(destination)
    }

    /// §13.2.4.1 Runtime Semantics: ArrayAccumulation
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-arrayaccumulation>
    fn compile_array_expression(
        &mut self,
        array: &oxc_ast::ast::ArrayExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let has_spread = array
            .elements
            .iter()
            .any(|el| matches!(el, oxc_ast::ast::ArrayExpressionElement::SpreadElement(_)));

        if has_spread {
            self.compile_array_expression_with_spread(array, module)
        } else {
            self.compile_array_expression_static(array, module)
        }
    }

    /// Fast path: no spread elements, all indices known at compile time.
    fn compile_array_expression_static(
        &mut self,
        array: &oxc_ast::ast::ArrayExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let destination = ValueLocation::temp(self.alloc_temp());
        let len =
            u16::try_from(array.elements.len()).map_err(|_| SourceLoweringError::TooManyLocals)?;
        self.instructions
            .push(Instruction::new_array(destination.register, len));
        let destination = self.stabilize_binding_value(destination)?;

        for (index, element) in array.elements.iter().enumerate() {
            let expr = match element {
                oxc_ast::ast::ArrayExpressionElement::SpreadElement(_) => {
                    unreachable!("spread elements handled by compile_array_expression_with_spread");
                }
                oxc_ast::ast::ArrayExpressionElement::Elision(_) => continue,
                expr => expr.to_expression(),
            };

            let value = self.compile_expression(expr, module)?;
            let value = if value.is_temp {
                self.stabilize_binding_value(value)?
            } else {
                value
            };
            let index_value = self
                .load_i32(i32::try_from(index).map_err(|_| SourceLoweringError::TooManyLocals)?)?;
            self.instructions.push(Instruction::set_index(
                destination.register,
                index_value.register,
                value.register,
            ));
            self.release(index_value);
            self.release(value);
        }

        Ok(destination)
    }

    /// Spread path: uses ArrayPush + SpreadIntoArray since indices are not
    /// statically known when spread elements are present.
    /// §13.2.4.1 ArrayAccumulation
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-arrayaccumulation>
    fn compile_array_expression_with_spread(
        &mut self,
        array: &oxc_ast::ast::ArrayExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // Allocate an empty array — final length unknown due to spread.
        let destination = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_array(destination.register, 0));
        let destination = self.stabilize_binding_value(destination)?;

        for element in &array.elements {
            match element {
                oxc_ast::ast::ArrayExpressionElement::SpreadElement(spread) => {
                    // §13.2.4.1 SpreadElement : ...AssignmentExpression
                    // 1. Let spreadRef = ? Evaluation of AssignmentExpression.
                    // 2. Let spreadObj = ? GetValue(spreadRef).
                    // 3. Let iteratorRecord = ? GetIterator(spreadObj, sync).
                    // 4. Repeat: IteratorStep → array.push each value.
                    let iterable = self.compile_expression(&spread.argument, module)?;
                    let iterable = if iterable.is_temp {
                        self.stabilize_binding_value(iterable)?
                    } else {
                        iterable
                    };
                    self.instructions.push(Instruction::spread_into_array(
                        destination.register,
                        iterable.register,
                    ));
                    self.release(iterable);
                }
                oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                    // §13.2.4.1 Elision — push undefined to preserve hole semantics
                    // in spread context. (Holes in spread arrays become explicit undefined.)
                    let undef = ValueLocation::temp(self.alloc_temp());
                    self.instructions
                        .push(Instruction::load_undefined(undef.register));
                    self.instructions.push(Instruction::array_push(
                        destination.register,
                        undef.register,
                    ));
                    self.release(undef);
                }
                expr => {
                    // §13.2.4.1 AssignmentExpression — single element.
                    let value = self.compile_expression(expr.to_expression(), module)?;
                    let value = if value.is_temp {
                        self.stabilize_binding_value(value)?
                    } else {
                        value
                    };
                    self.instructions.push(Instruction::array_push(
                        destination.register,
                        value.register,
                    ));
                    self.release(value);
                }
            }
        }

        Ok(destination)
    }

    fn compile_static_member_expression(
        &mut self,
        member: &StaticMemberExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // §13.3.7 SuperProperty — `super.foo` inside a method.
        // Spec: <https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation>
        if matches!(&member.object, Expression::Super(_)) {
            let property = self.intern_property_name(member.property.name.as_str())?;
            let result = ValueLocation::temp(self.alloc_temp());
            self.record_location(member.span);
            self.instructions
                .push(Instruction::get_super_property(result.register, property));
            return Ok(result);
        }
        let object = self.compile_expression(&member.object, module)?;
        let result = if object.is_temp {
            object
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        let property = self.intern_property_name(member.property.name.as_str())?;
        // Re-record the member-access span right before emit so that any
        // TypeError thrown by `GetProperty` (e.g., `null.foo`) underlines
        // the access expression itself, not whatever sub-expression
        // happened to set the active location last.
        self.record_location(member.span);
        self.instructions.push(Instruction::get_property(
            result.register,
            object.register,
            property,
        ));
        if result.register != object.register {
            self.release(object);
        }
        Ok(result)
    }

    fn compile_computed_member_expression(
        &mut self,
        member: &ComputedMemberExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // §13.3.7 SuperProperty — `super[key]` inside a method.
        if matches!(&member.object, Expression::Super(_)) {
            // §13.3.7.1 step 2: GetSuperBase checks [[ThisBindingStatus]]
            // BEFORE evaluating the key expression. For derived ctors where
            // `this` is still uninitialized (hole), this throws ReferenceError.
            if self.is_derived_constructor {
                if let Ok(Binding::ThisRegister(reg) | Binding::ImmutableRegister(reg)) =
                    self.resolve_binding("this")
                {
                    self.emit_assert_not_hole(reg);
                } else if let Ok(Binding::ThisUpvalue(uv) | Binding::ImmutableUpvalue(uv)) =
                    self.resolve_binding("this")
                {
                    let tmp = self.alloc_temp();
                    self.instructions.push(Instruction::get_upvalue(tmp, uv));
                    self.emit_assert_not_hole(tmp);
                    self.release(ValueLocation::temp(tmp));
                }
            }
            let key = self.compile_expression(&member.expression, module)?;
            let result = ValueLocation::temp(self.alloc_temp());
            self.record_location(member.span);
            self.instructions
                .push(Instruction::get_super_property_computed(
                    result.register,
                    key.register,
                ));
            self.release(key);
            return Ok(result);
        }
        let mut object = self.compile_expression(&member.object, module)?;
        if object.is_temp {
            object = self.stabilize_binding_value(object)?;
        }
        let result = if object.is_temp {
            object
        } else {
            ValueLocation::temp(self.alloc_temp())
        };

        match &member.expression {
            Expression::StringLiteral(literal) => {
                let property = self.intern_property_name(literal.value.as_str())?;
                self.record_location(member.span);
                self.instructions.push(Instruction::get_property(
                    result.register,
                    object.register,
                    property,
                ));
            }
            _ => {
                let index = self.compile_expression(&member.expression, module)?;
                self.record_location(member.span);
                self.instructions.push(Instruction::get_index(
                    result.register,
                    object.register,
                    index.register,
                ));
                self.release(index);
            }
        }

        if result.register != object.register {
            self.release(object);
        }
        Ok(result)
    }

    fn compile_delete_expression(
        &mut self,
        argument: &Expression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // §13.5.1.1 Static Semantics: Early Errors for `delete UnaryExpression`.
        // It is a Syntax Error if the derived UnaryExpression is a
        // `MemberExpression . PrivateIdentifier` or a
        // `CallExpression . PrivateIdentifier` — in either case, peeled
        // through any ParenthesizedExpression covers.
        // Spec: <https://tc39.es/ecma262/#sec-delete-operator-static-semantics-early-errors>
        {
            let mut peeled = argument;
            while let Expression::ParenthesizedExpression(paren) = peeled {
                peeled = &paren.expression;
            }
            if matches!(peeled, Expression::PrivateFieldExpression(_)) {
                return Err(SourceLoweringError::EarlyError(
                    "`delete` may not be applied to a private field reference".to_string(),
                ));
            }
        }
        match argument {
            Expression::StaticMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let result = if object.is_temp {
                    object
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.instructions.push(Instruction::delete_property(
                    result.register,
                    object.register,
                    property,
                ));
                if result.register != object.register {
                    self.release(object);
                }
                Ok(result)
            }
            Expression::ComputedMemberExpression(member) => {
                // Optimise: if the key is a string literal, use the named delete path.
                if let Expression::StringLiteral(literal) = &member.expression {
                    let object = self.compile_expression(&member.object, module)?;
                    let result = if object.is_temp {
                        object
                    } else {
                        ValueLocation::temp(self.alloc_temp())
                    };
                    let property = self.intern_property_name(literal.value.as_str())?;
                    self.instructions.push(Instruction::delete_property(
                        result.register,
                        object.register,
                        property,
                    ));
                    if result.register != object.register {
                        self.release(object);
                    }
                    return Ok(result);
                }

                // General case: dynamic key — emit DeleteComputed.
                let mut object = self.compile_expression(&member.object, module)?;
                if object.is_temp {
                    object = self.stabilize_binding_value(object)?;
                }
                let key = self.compile_expression(&member.expression, module)?;
                let result = if object.is_temp {
                    object
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions.push(Instruction::delete_computed(
                    result.register,
                    object.register,
                    key.register,
                ));
                self.release(key);
                if result.register != object.register {
                    self.release(object);
                }
                Ok(result)
            }
            // §13.5.1.2 — `delete identifier`: in sloppy mode, attempt to delete
            // the binding. In strict mode, this is a SyntaxError (caught by parser).
            // For simplicity, we return `true` (non-configurable bindings are rare
            // in VM-compiled code; this matches the "always succeeds" behavior
            // for undeclared globals).
            Expression::Identifier(_) => self.compile_bool(true),
            // §13.5.1.2 — `delete <non-reference>`: evaluate the expression
            // for side effects, then return `true`.
            _ => {
                let value = self.compile_expression(argument, module)?;
                self.release(value);
                self.compile_bool(true)
            }
        }
    }

    fn compile_conditional_expression(
        &mut self,
        conditional: &oxc_ast::ast::ConditionalExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let test = self.compile_expression(&conditional.test, module)?;
        let jump_to_alternate =
            self.emit_conditional_placeholder(Opcode::JumpIfFalse, test.register);
        self.release(test);

        let consequent = self.compile_expression(&conditional.consequent, module)?;
        let result = if consequent.is_temp {
            consequent
        } else {
            let result = ValueLocation::temp(self.alloc_temp());
            self.instructions
                .push(Instruction::move_(result.register, consequent.register));
            self.release(consequent);
            result
        };
        let jump_to_end = self.emit_jump_placeholder();

        self.patch_jump(jump_to_alternate, self.instructions.len())?;

        let alternate = self.compile_expression(&conditional.alternate, module)?;
        self.instructions
            .push(Instruction::move_(result.register, alternate.register));
        if alternate.register != result.register {
            self.release(alternate);
        }

        self.patch_jump(jump_to_end, self.instructions.len())?;

        Ok(result)
    }

    /// Sequence expression: `(a, b, c)` — evaluates all, returns last.
    fn compile_sequence_expression(
        &mut self,
        sequence: &oxc_ast::ast::SequenceExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let mut result = None;
        for expression in &sequence.expressions {
            if let Some(prev) = result {
                self.release(prev);
            }
            result = Some(self.compile_expression(expression, module)?);
        }
        result.ok_or_else(|| {
            SourceLoweringError::Unsupported("empty sequence expression".to_string())
        })
    }

    /// §14.4 Yield — `yield expr` or bare `yield` (produces undefined).
    /// Spec: <https://tc39.es/ecma262/#sec-yield>
    ///
    /// Emits `Yield dst, value` which suspends the generator.
    /// On resume, the sent value (from `.next(v)`) is written to `dst`.
    fn compile_yield_expression(
        &mut self,
        yield_expr: &oxc_ast::ast::YieldExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if yield_expr.delegate {
            // §14.4.4 yield* — delegate to sub-iterator.
            // Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>
            let argument = yield_expr.argument.as_ref().ok_or_else(|| {
                SourceLoweringError::Unsupported("yield* requires an argument".to_string())
            })?;
            let iterable = self.compile_expression(argument, module)?;
            let iterator = self.allocate_local()?;
            self.instructions
                .push(Instruction::get_iterator(iterator, iterable.register));
            self.release(iterable);
            let dst = self.allocate_local()?;
            self.instructions
                .push(Instruction::yield_star(dst, iterator));
            return Ok(ValueLocation::local(dst));
        }

        let value = if let Some(argument) = &yield_expr.argument {
            self.compile_expression(argument, module)?
        } else {
            self.load_undefined()?
        };

        // Yield suspends execution and returns `value` to the caller.
        // The register `dst` will receive the sent value when resumed.
        let dst = self.allocate_local()?;
        self.instructions
            .push(Instruction::yield_(dst, value.register));
        self.release(value);
        Ok(ValueLocation::local(dst))
    }

    /// Compiles `await expr` — §14.7 Await.
    /// Spec: <https://tc39.es/ecma262/#sec-await>
    ///
    /// Emits `Await dst, src` which suspends the async function.
    /// On resume, the awaited result is written to `dst`.
    fn compile_await_expression(
        &mut self,
        await_expr: &oxc_ast::ast::AwaitExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let operand = self.compile_expression(&await_expr.argument, module)?;
        let dst = self.allocate_local()?;
        self.instructions
            .push(Instruction::r#await(dst, operand.register));
        self.release(operand);
        Ok(ValueLocation::local(dst))
    }

    /// ��13.3.7 Optional Chaining — `obj?.prop`, `obj?.[key]`, `obj?.method()`
    /// Spec: <https://tc39.es/ecma262/#sec-optional-chaining>
    ///
    /// Strategy: extract the base object, check if nullish, short-circuit to
    /// undefined if so. Otherwise perform the member access / call normally.
    /// All nullish guards within a single chain share the same short-circuit
    /// target (the end label) per §13.3.7.1.
    fn compile_chain_expression(
        &mut self,
        chain: &oxc_ast::ast::ChainExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        use oxc_ast::ast::ChainElement;

        // Pre-allocate result with `undefined` for the short-circuit path.
        let result = self.allocate_local()?;
        self.instructions.push(Instruction::load_undefined(result));

        // All nullish guards jump to the same end label.
        let mut jump_patches: Vec<usize> = Vec::new();

        match &chain.expression {
            ChainElement::StaticMemberExpression(member) => {
                let base = self.compile_expression(&member.object, module)?;
                let base = self.stabilize_binding_value(base)?;

                self.emit_nullish_guard(base.register, &mut jump_patches)?;

                let prop = self.intern_property_name(member.property.name.as_str())?;
                let val = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::get_property(
                    val.register,
                    base.register,
                    prop,
                ));
                self.instructions
                    .push(Instruction::move_(result, val.register));
                self.release(val);
            }
            ChainElement::ComputedMemberExpression(member) => {
                let base = self.compile_expression(&member.object, module)?;
                let base = self.stabilize_binding_value(base)?;

                self.emit_nullish_guard(base.register, &mut jump_patches)?;

                let key = self.compile_expression(&member.expression, module)?;
                let val = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::get_index(
                    val.register,
                    base.register,
                    key.register,
                ));
                self.release(key);
                self.instructions
                    .push(Instruction::move_(result, val.register));
                self.release(val);
            }
            // §13.3.8.1 Optional call: `a?.()`, `a?.b()`, `a.b?.()`, `a?.b?.()`
            // Spec: <https://tc39.es/ecma262/#sec-optional-chaining>
            ChainElement::CallExpression(call) => {
                // Compile callee with chain-aware optional handling.
                let (callee, receiver) =
                    self.compile_chain_call_target(&call.callee, &mut jump_patches, module)?;

                // Stabilize for safe register reuse across argument compilation.
                let receiver = match receiver {
                    Some(r) if r.is_temp => Some(self.stabilize_binding_value(r)?),
                    other => other,
                };
                let callee = if callee.is_temp {
                    self.stabilize_binding_value(callee)?
                } else {
                    callee
                };

                // If the call itself uses `?.()` syntax, check callee for nullish.
                if call.optional {
                    self.emit_nullish_guard(callee.register, &mut jump_patches)?;
                }

                // Compile call arguments and emit call instruction.
                let has_spread = call
                    .arguments
                    .iter()
                    .any(|arg| matches!(arg, Argument::SpreadElement(_)));

                let call_result = if has_spread {
                    self.compile_call_with_spread(&call.arguments, callee, receiver, false, module)?
                } else {
                    self.compile_call_static_args(&call.arguments, callee, receiver, false, module)?
                };

                self.instructions
                    .push(Instruction::move_(result, call_result.register));
                if call_result.register != result {
                    self.release(call_result);
                }
            }
            _ => {
                return Err(SourceLoweringError::Unsupported(
                    "unsupported chain element type".to_string(),
                ));
            }
        }

        let end = self.instructions.len();
        for patch in jump_patches {
            self.patch_jump(patch, end)?;
        }
        Ok(ValueLocation::local(result))
    }

    /// Compile the callee of an optional chain call expression.
    ///
    /// When the callee is a member expression with `optional: true` (e.g. the
    /// `a?.b` in `a?.b()`), a nullish guard is inserted for the base object.
    /// Returns `(callee, receiver)` just like `compile_call_target`.
    ///
    /// §13.3.7.1 Runtime Semantics: ChainEvaluation
    /// Spec: <https://tc39.es/ecma262/#sec-optional-chaining-chain-evaluation>
    fn compile_chain_call_target(
        &mut self,
        callee: &Expression<'_>,
        jump_patches: &mut Vec<usize>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(ValueLocation, Option<ValueLocation>), SourceLoweringError> {
        match callee {
            // `a?.b()` — callee is StaticMember with optional flag
            Expression::StaticMemberExpression(member) if member.optional => {
                let receiver = self.compile_expression(&member.object, module)?;
                let receiver = self.stabilize_binding_value(receiver)?;

                self.emit_nullish_guard(receiver.register, jump_patches)?;

                let callee_reg = self.alloc_temp();
                let prop = self.intern_property_name(member.property.name.as_str())?;
                self.instructions.push(Instruction::get_property(
                    callee_reg,
                    receiver.register,
                    prop,
                ));
                Ok((ValueLocation::temp(callee_reg), Some(receiver)))
            }
            // `a?.[key]()` — callee is ComputedMember with optional flag
            Expression::ComputedMemberExpression(member) if member.optional => {
                let receiver = self.compile_expression(&member.object, module)?;
                let receiver = self.stabilize_binding_value(receiver)?;

                self.emit_nullish_guard(receiver.register, jump_patches)?;

                let key = self.compile_expression(&member.expression, module)?;
                let callee_reg = self.alloc_temp();
                self.instructions.push(Instruction::get_index(
                    callee_reg,
                    receiver.register,
                    key.register,
                ));
                self.release(key);
                Ok((ValueLocation::temp(callee_reg), Some(receiver)))
            }
            // Non-optional callee — compile normally via standard call target.
            _ => self.compile_call_target(callee, module),
        }
    }

    /// Emit a nullish guard: `if (value == null) jump to end`.
    ///
    /// Uses abstract equality (`==`) so that both `null` and `undefined` match.
    /// The jump placeholder index is pushed to `jump_patches` for later patching.
    ///
    /// §13.3.7.1 Runtime Semantics: ChainEvaluation
    /// Spec: <https://tc39.es/ecma262/#sec-optional-chaining-chain-evaluation>
    fn emit_nullish_guard(
        &mut self,
        value: BytecodeRegister,
        jump_patches: &mut Vec<usize>,
    ) -> Result<(), SourceLoweringError> {
        let null_val = self.load_null()?;
        let is_nullish = ValueLocation::temp(self.alloc_temp());
        self.instructions.push(Instruction::loose_eq(
            is_nullish.register,
            value,
            null_val.register,
        ));
        self.release(null_val);
        let patch = self.emit_conditional_placeholder(Opcode::JumpIfTrue, is_nullish.register);
        self.release(is_nullish);
        jump_patches.push(patch);
        Ok(())
    }

    /// Template literal: `` `prefix${expr}mid${expr}suffix` ``
    /// Compiles to a chain of string concatenations via Add.
    fn compile_template_literal(
        &mut self,
        template: &oxc_ast::ast::TemplateLiteral<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        // quasis[0] expr[0] quasis[1] expr[1] ... quasis[N]
        // quasis always has one more element than expressions.
        let first_quasi = &template.quasis[0];
        let cooked = first_quasi
            .value
            .cooked
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or("");
        let mut result = self.compile_string_literal(cooked)?;

        for (i, expression) in template.expressions.iter().enumerate() {
            let expr_val = self.compile_expression(expression, module)?;
            let expr_string = ValueLocation::temp(self.alloc_temp());
            self.instructions.push(Instruction::to_string(
                expr_string.register,
                expr_val.register,
            ));
            self.release(expr_val);
            let dst = if result.is_temp {
                result
            } else {
                ValueLocation::temp(self.alloc_temp())
            };
            self.instructions.push(Instruction::add(
                dst.register,
                result.register,
                expr_string.register,
            ));
            if dst.register != result.register {
                self.release(result);
            }
            self.release(expr_string);
            result = dst;

            // Append the next quasi (string part after the expression).
            let quasi = &template.quasis[i + 1];
            let quasi_str = quasi
                .value
                .cooked
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("");
            if !quasi_str.is_empty() {
                let str_val = self.compile_string_literal(quasi_str)?;
                let dst = if result.is_temp {
                    result
                } else {
                    ValueLocation::temp(self.alloc_temp())
                };
                self.instructions.push(Instruction::add(
                    dst.register,
                    result.register,
                    str_val.register,
                ));
                if dst.register != result.register {
                    self.release(result);
                }
                self.release(str_val);
                result = dst;
            }
        }

        Ok(result)
    }

    /// §13.3.11 Tagged Templates — `` tag`hello ${expr} world` ``
    /// Spec: <https://tc39.es/ecma262/#sec-tagged-templates>
    ///
    /// Compiles a tagged template expression by:
    /// 1. Building the template object (frozen array of cooked strings with `.raw`)
    /// 2. Evaluating each substitution expression
    /// 3. Calling the tag function: `tag(templateObj, sub0, sub1, ...)`
    ///
    /// §13.2.8.3 GetTemplateObject
    /// Spec: <https://tc39.es/ecma262/#sec-gettemplateobject>
    fn compile_tagged_template_expression(
        &mut self,
        tagged: &oxc_ast::ast::TaggedTemplateExpression<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let template = &tagged.quasi;

        // 1. Compile the tag expression (callee + optional receiver for method calls).
        let (callee, receiver) = self.compile_call_target(&tagged.tag, module)?;
        let receiver = match receiver {
            Some(r) if r.is_temp => Some(self.stabilize_binding_value(r)?),
            other => other,
        };
        let callee = if callee.is_temp {
            self.stabilize_binding_value(callee)?
        } else {
            callee
        };

        // 2. Build the template strings array (cooked values).
        //    §13.2.8.3 GetTemplateObject — `template` is an Array of cooked strings.
        let quasis_count = template.quasis.len() as u16;
        let strings_arr = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_array(strings_arr.register, quasis_count));
        let strings_arr = self.stabilize_binding_value(strings_arr)?;

        for (i, quasi) in template.quasis.iter().enumerate() {
            let cooked = quasi.value.cooked.as_ref().map(|s| s.as_str());
            let val = match cooked {
                Some(s) => self.compile_string_literal(s)?,
                None => {
                    // Invalid escape sequence → cooked is undefined.
                    let v = ValueLocation::temp(self.alloc_temp());
                    self.instructions
                        .push(Instruction::load_undefined(v.register));
                    v
                }
            };
            let idx = self.compile_numeric_literal(i as f64)?;
            self.instructions.push(Instruction::set_index(
                strings_arr.register,
                idx.register,
                val.register,
            ));
            self.release(idx);
            self.release(val);
        }

        // 3. Build the raw strings array.
        //    §13.2.8.3 GetTemplateObject — `template.raw` is an Array of raw strings.
        let raw_arr = ValueLocation::temp(self.alloc_temp());
        self.instructions
            .push(Instruction::new_array(raw_arr.register, quasis_count));
        let raw_arr = self.stabilize_binding_value(raw_arr)?;

        for (i, quasi) in template.quasis.iter().enumerate() {
            let raw_str = quasi.value.raw.as_str();
            let val = self.compile_string_literal(raw_str)?;
            let idx = self.compile_numeric_literal(i as f64)?;
            self.instructions.push(Instruction::set_index(
                raw_arr.register,
                idx.register,
                val.register,
            ));
            self.release(idx);
            self.release(val);
        }

        // 4. Set `strings.raw = rawArray`.
        let raw_prop = self.intern_property_name("raw")?;
        self.instructions.push(Instruction::set_property(
            strings_arr.register,
            raw_arr.register,
            raw_prop,
        ));
        self.release(raw_arr);

        // 5. Evaluate substitution expressions.
        let mut sub_values = Vec::with_capacity(template.expressions.len());
        for expr in &template.expressions {
            let val = self.compile_expression(expr, module)?;
            sub_values.push(if val.is_temp {
                self.stabilize_binding_value(val)?
            } else {
                val
            });
        }

        // 6. Call: tag(strings, sub0, sub1, ...)
        //    Total argument count = 1 (template object) + substitution count.
        let total_args = 1 + sub_values.len();
        let argument_count =
            RegisterIndex::try_from(total_args).map_err(|_| SourceLoweringError::TooManyLocals)?;
        let arg_start = self.reserve_temp_window(argument_count)?;

        // First arg: template strings array
        self.instructions
            .push(Instruction::move_(arg_start, strings_arr.register));
        self.release(strings_arr);

        // Remaining args: substitution values
        for (i, val) in sub_values.into_iter().enumerate() {
            let dst = BytecodeRegister::new(arg_start.index() + 1 + i as u16);
            if val.register != dst {
                self.instructions
                    .push(Instruction::move_(dst, val.register));
                self.release(val);
            }
        }

        let mut call_result = if receiver.is_some_and(|r| r.is_temp) {
            receiver.expect("receiver must exist")
        } else if callee.is_temp {
            callee
        } else {
            ValueLocation::temp(self.alloc_temp())
        };
        let pc = self.instructions.len();
        self.instructions.push(Instruction::call_closure(
            call_result.register,
            callee.register,
            arg_start,
        ));
        let call_site = match receiver {
            Some(recv) => CallSite::Closure(ClosureCall::new_with_receiver(
                argument_count,
                FrameFlags::new(false, true, false),
                recv.register,
            )),
            None => CallSite::Closure(ClosureCall::new(
                argument_count,
                FrameFlags::new(false, true, false),
            )),
        };
        self.record_call_site(pc, call_site);

        // Move result to highest temp (same pattern as compile_call_static_args).
        if argument_count != 0 {
            let stable_register =
                BytecodeRegister::new(arg_start.index() + argument_count.saturating_sub(1));
            if call_result.register != stable_register {
                self.instructions
                    .push(Instruction::move_(stable_register, call_result.register));
                call_result = ValueLocation::temp(stable_register);
            }
            self.release_temp_window(argument_count.saturating_sub(1));
        }
        if callee.register != call_result.register {
            self.release(callee);
        }
        if let Some(recv) = receiver
            && recv.register != call_result.register
        {
            self.release(recv);
        }

        Ok(call_result)
    }

    /// §12.9 Regular Expression Literals — emits a `NewRegExp` instruction.
    ///
    /// Compiles a BigInt literal (`42n`) by stripping the trailing `n` suffix,
    /// interning the decimal value in the BigInt constant pool, and emitting
    /// `LoadBigInt dst, bigint_id`.
    ///
    /// §6.1.6.2 The BigInt Type
    /// Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    fn compile_bigint_literal(&mut self, raw: &str) -> Result<ValueLocation, SourceLoweringError> {
        // §12.8.6 NumericLiteral :: BigIntLiteralSuffixed
        // Spec: <https://tc39.es/ecma262/#sec-numeric-literals>
        // The raw text includes the trailing `n` suffix and may use
        // prefixes (0x/0o/0b) and numeric-literal separators (`_`).
        // Normalize to the canonical decimal form so that the rest of
        // the runtime (num_bigint parsing, SameValue, arithmetic) sees
        // a consistent representation.
        let decimal = normalize_bigint_literal(raw).map_err(|err| {
            SourceLoweringError::Unsupported(format!("invalid BigInt literal {raw}: {err}"))
        })?;
        self.compile_bigint_literal_value(&decimal)
    }

    /// Compiles a BigInt value (already normalized to a decimal string,
    /// optionally signed) into a LoadBigInt.
    fn compile_bigint_literal_value(
        &mut self,
        value: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let bigint_id = self.intern_bigint(value)?;
        let dst = self.alloc_temp();
        self.instructions
            .push(Instruction::load_bigint(dst, bigint_id));
        Ok(ValueLocation::temp(dst))
    }

    /// Interns the (pattern, flags) pair into the function's regexp side table
    /// and emits `NewRegExp dst, regexp_id` which creates a fresh RegExp object
    /// at runtime.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-literals-regular-expression-literals>
    fn compile_regexp_literal(
        &mut self,
        pattern: &str,
        flags: &str,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let regexp_id = self.intern_regexp(pattern, flags)?;
        let dst = self.allocate_local()?;
        self.instructions
            .push(Instruction::new_regexp(dst, regexp_id));
        Ok(ValueLocation::local(dst))
    }

    /// §13.3.10 Dynamic `import()` — compiles `import(specifier)` to a
    /// DynamicImport instruction that returns a Promise for the module namespace.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-import-calls>
    fn compile_import_expression(
        &mut self,
        import_expr: &oxc_ast::ast::ImportExpression<'_>,
        module: &mut super::module_compiler::ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        let specifier = self.compile_expression(&import_expr.source, module)?;
        let specifier_reg = specifier.register;

        // Discard options argument if present (not yet supported).
        if let Some(options) = &import_expr.options {
            let opt = self.compile_expression(options, module)?;
            self.release(opt);
        }

        let dst = self.allocate_local()?;
        self.instructions
            .push(Instruction::dynamic_import(dst, specifier_reg));
        self.release(specifier);
        Ok(ValueLocation::local(dst))
    }

    /// §13.3.12 MetaProperty — `import.meta` or `new.target`.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-meta-properties>
    fn compile_meta_property(
        &mut self,
        meta: &oxc_ast::ast::MetaProperty<'_>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if meta.meta.name == "import" && meta.property.name == "meta" {
            // import.meta — emit ImportMeta instruction.
            let dst = self.allocate_local()?;
            self.instructions.push(Instruction::import_meta(dst));
            Ok(ValueLocation::local(dst))
        } else if meta.meta.name == "new" && meta.property.name == "target" {
            // §13.3.12.1 `new.target` — returns the constructor being
            // invoked via `new`, or `undefined` for regular calls. Arrows
            // lexically inherit from the enclosing construct context.
            // Spec: <https://tc39.es/ecma262/#sec-meta-properties-runtime-semantics-evaluation>
            let dst = self.alloc_temp();
            self.instructions.push(Instruction::load_new_target(dst));
            Ok(ValueLocation::temp(dst))
        } else {
            Err(SourceLoweringError::Unsupported(format!(
                "meta property {}.{}",
                meta.meta.name, meta.property.name
            )))
        }
    }
}

/// §B.1.1 — Detects legacy octal numeric literals (e.g. `077`).
///
/// Legacy octals start with `0` followed by octal digits, but NOT `0o`, `0x`, `0b`, or `0.`.
/// Modern `0o777` is always valid.
///
/// Spec: <https://tc39.es/ecma262/#sec-additional-syntax-numeric-literals>
fn is_legacy_octal_literal(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'0' {
        return false;
    }
    // Modern prefixes: 0x, 0X, 0b, 0B, 0o, 0O, 0., 0e, 0E, 0n (bigint)
    matches!(bytes[1], b'0'..=b'7')
        && !matches!(
            bytes[1],
            b'x' | b'X' | b'b' | b'B' | b'o' | b'O' | b'.' | b'e' | b'E' | b'n'
        )
}

/// §B.1.2 — Detects legacy octal escape sequences in raw string literals.
///
/// Scans the raw string (including quotes) for `\1`..`\7`, `\8`, `\9`,
/// or `\0` followed by another digit. Returns the first offending sequence
/// if found. `\0` alone (without a following digit) is allowed.
///
/// Spec: <https://tc39.es/ecma262/#sec-additional-syntax-string-literals>
fn find_legacy_octal_escape(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            match next {
                // \1 through \7 are always legacy octal escapes
                b'1'..=b'7' => {
                    return Some(format!("\\{}", next as char));
                }
                // \8, \9 are legacy non-octal decimal escapes (also banned)
                b'8' | b'9' => {
                    return Some(format!("\\{}", next as char));
                }
                // \0 is allowed ONLY if NOT followed by another digit
                b'0' => {
                    if i + 2 < bytes.len() && bytes[i + 2].is_ascii_digit() {
                        return Some(format!("\\0{}", bytes[i + 2] as char));
                    }
                    // \0 alone is fine (null character)
                    i += 2;
                    continue;
                }
                _ => {
                    i += 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    None
}

/// Normalize a BigInt literal source string (e.g. `0x0_Fn`, `0b1_0n`,
/// `1_000n`) to its canonical unsigned decimal string representation.
///
/// The returned string is what the runtime stores and what all arithmetic
/// and comparison paths parse with `num_bigint::BigInt::from_str`. Handles:
/// - trailing `n` suffix removal;
/// - numeric-literal separators (`_`) per §12.8.6;
/// - base prefixes `0x`/`0X` (16), `0o`/`0O` (8), `0b`/`0B` (2);
/// - decimal literals (default).
///
/// Spec: <https://tc39.es/ecma262/#sec-literals-numeric-literals>
fn normalize_bigint_literal(raw: &str) -> Result<String, String> {
    use num_bigint::BigInt;
    use num_traits::Num;

    let body = raw.strip_suffix('n').unwrap_or(raw);
    if body.is_empty() {
        return Err("empty BigInt literal".to_string());
    }

    // Strip numeric-literal separators. Lexer rules already guarantee
    // the separators appear only between digits, so a naive filter is
    // sufficient at this stage — oxc has already validated the grammar.
    let cleaned: String = body.chars().filter(|c| *c != '_').collect();

    let (radix, digits) = if let Some(rest) = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
    {
        (16, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0o")
        .or_else(|| cleaned.strip_prefix("0O"))
    {
        (8, rest)
    } else if let Some(rest) = cleaned
        .strip_prefix("0b")
        .or_else(|| cleaned.strip_prefix("0B"))
    {
        (2, rest)
    } else {
        (10, cleaned.as_str())
    };

    if digits.is_empty() {
        return Err("BigInt literal has no digits".to_string());
    }

    let value = BigInt::from_str_radix(digits, radix)
        .map_err(|err| format!("parse error (radix {radix}): {err}"))?;

    Ok(value.to_string())
}
