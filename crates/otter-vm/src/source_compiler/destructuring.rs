use super::module_compiler::ModuleCompiler;
use super::shared::{Binding, FunctionCompiler, ValueLocation};
use super::*;

impl<'a> FunctionCompiler<'a> {
    /// Destructure an object pattern `{ a, b: c, d = default }` from `source_register`.
    pub(super) fn compile_object_destructuring(
        &mut self,
        pattern: &oxc_ast::ast::ObjectPattern<'_>,
        source_register: BytecodeRegister,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        for prop in &pattern.properties {
            let key_name =
                super::ast::non_computed_property_key_name(&prop.key).ok_or_else(|| {
                    SourceLoweringError::Unsupported(
                        "computed destructuring property keys".to_string(),
                    )
                })?;

            let prop_id = self.intern_property_name(&key_name)?;
            let extracted = self.alloc_temp();
            self.instructions.push(Instruction::get_property(
                extracted,
                source_register,
                prop_id,
            ));
            let extracted_loc = ValueLocation::temp(extracted);

            self.compile_binding_pattern_target(
                &prop.value,
                extracted_loc,
                prop.shorthand,
                module,
            )?;
        }

        if pattern.rest.is_some() {
            return Err(SourceLoweringError::Unsupported(
                "rest elements in object destructuring".to_string(),
            ));
        }

        Ok(())
    }

    /// Destructure an array pattern `[a, b, , d = default]` from `source_register`.
    pub(super) fn compile_array_destructuring(
        &mut self,
        pattern: &oxc_ast::ast::ArrayPattern<'_>,
        source_register: BytecodeRegister,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        if pattern.rest.is_some() {
            return Err(SourceLoweringError::Unsupported(
                "rest elements in array destructuring".to_string(),
            ));
        }

        for (index, element) in pattern.elements.iter().enumerate() {
            let Some(element) = element else {
                // Hole in array pattern — skip.
                continue;
            };

            let i32_index = i32::try_from(index).map_err(|_| SourceLoweringError::TooManyLocals)?;
            let index_val = self.load_i32(i32_index)?;
            let extracted = self.alloc_temp();
            self.instructions.push(Instruction::get_index(
                extracted,
                source_register,
                index_val.register,
            ));
            self.release(index_val);
            let extracted_loc = ValueLocation::temp(extracted);

            self.compile_binding_pattern_target(element, extracted_loc, false, module)?;
        }

        Ok(())
    }

    /// Bind a single destructuring target (handles nested patterns, defaults, identifiers).
    pub(super) fn compile_binding_pattern_target(
        &mut self,
        pattern: &BindingPattern<'_>,
        value: ValueLocation,
        _shorthand: bool,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        match pattern {
            BindingPattern::BindingIdentifier(identifier) => {
                let name = identifier.name.as_str();
                let register = if let Ok(existing) = self.resolve_binding(name) {
                    match existing {
                        Binding::Register(r) => r,
                        Binding::Function { closure_register } => closure_register,
                        Binding::Upvalue(_) => {
                            self.instructions
                                .push(Instruction::set_upvalue(value.register, {
                                    if let Binding::Upvalue(u) = existing {
                                        u
                                    } else {
                                        unreachable!()
                                    }
                                }));
                            self.release(value);
                            return Ok(());
                        }
                    }
                } else {
                    self.declare_variable_binding(name, true)?
                };
                if value.register != register {
                    self.instructions
                        .push(Instruction::move_(register, value.register));
                    self.release(value);
                }
                Ok(())
            }
            BindingPattern::AssignmentPattern(assignment) => {
                // Apply default: if value is undefined, use default expression.
                let materialized = self.materialize_value(value);
                let undef = self.load_undefined()?;
                let cmp = ValueLocation::temp(self.alloc_temp());
                self.instructions.push(Instruction::eq(
                    cmp.register,
                    materialized.register,
                    undef.register,
                ));
                self.release(undef);
                let jump_skip =
                    self.emit_conditional_placeholder(Opcode::JumpIfFalse, cmp.register);
                self.release(cmp);
                let default_val = self.compile_expression(&assignment.right, module)?;
                if default_val.register != materialized.register {
                    self.instructions.push(Instruction::move_(
                        materialized.register,
                        default_val.register,
                    ));
                    self.release(default_val);
                }
                self.patch_jump(jump_skip, self.instructions.len())?;
                // Now bind the (possibly defaulted) value to the inner pattern.
                self.compile_binding_pattern_target(&assignment.left, materialized, false, module)
            }
            BindingPattern::ObjectPattern(object_pattern) => {
                let materialized = self.materialize_value(value);
                self.compile_object_destructuring(object_pattern, materialized.register, module)?;
                self.release(materialized);
                Ok(())
            }
            BindingPattern::ArrayPattern(array_pattern) => {
                let materialized = self.materialize_value(value);
                self.compile_array_destructuring(array_pattern, materialized.register, module)?;
                self.release(materialized);
                Ok(())
            }
        }
    }
}
