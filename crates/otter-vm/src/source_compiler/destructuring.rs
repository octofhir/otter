use super::module_compiler::ModuleCompiler;
use super::shared::{Binding, FunctionCompiler, ValueLocation};
use super::*;
use crate::bytecode::ProgramCounter;

impl<'a> FunctionCompiler<'a> {
    fn emit_iterator_value_or_undefined(
        &mut self,
        iterator_register: BytecodeRegister,
        done_register: BytecodeRegister,
        step_value_register: BytecodeRegister,
        extracted_register: BytecodeRegister,
    ) -> Result<(), SourceLoweringError> {
        self.instructions.push(Instruction::iterator_next(
            done_register,
            step_value_register,
            iterator_register,
        ));
        let undef = self.load_undefined()?;
        self.instructions
            .push(Instruction::move_(extracted_register, undef.register));
        self.release(undef);
        let jump_skip = self.emit_conditional_placeholder(Opcode::JumpIfTrue, done_register);
        self.instructions
            .push(Instruction::move_(extracted_register, step_value_register));
        self.patch_jump(jump_skip, self.instructions.len())?;
        Ok(())
    }

    fn emit_destructuring_iterator_handler(
        &mut self,
        try_start: usize,
        try_end: usize,
        iterator_register: BytecodeRegister,
    ) -> Result<(), SourceLoweringError> {
        let exception_register = self.allocate_local()?;
        let jump_over_exception_handler = self.emit_jump_placeholder();
        let exception_handler_pc = self.instructions.len();
        self.instructions
            .push(Instruction::load_exception(exception_register));
        self.instructions
            .push(Instruction::iterator_close(iterator_register));
        self.instructions
            .push(Instruction::throw(exception_register));
        self.patch_jump(jump_over_exception_handler, self.instructions.len())?;

        self.exception_handlers.push(ExceptionHandler::new(
            try_start as ProgramCounter,
            try_end as ProgramCounter,
            exception_handler_pc as ProgramCounter,
        ));
        Ok(())
    }

    fn compile_assignment_property_key_value(
        &mut self,
        property: &oxc_ast::ast::AssignmentTargetPropertyProperty<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if property.computed {
            return self.compile_expression(property.name.to_expression(), module);
        }

        let key_name =
            super::ast::non_computed_property_key_name(&property.name).ok_or_else(|| {
                SourceLoweringError::Unsupported("object assignment property key".to_string())
            })?;
        self.compile_string_literal(&key_name)
    }

    fn compile_object_property_key_value(
        &mut self,
        property: &oxc_ast::ast::BindingProperty<'_>,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<ValueLocation, SourceLoweringError> {
        if property.computed {
            return self.compile_expression(property.key.to_expression(), module);
        }

        let key_name =
            super::ast::non_computed_property_key_name(&property.key).ok_or_else(|| {
                SourceLoweringError::Unsupported("object destructuring property key".to_string())
            })?;
        self.compile_string_literal(&key_name)
    }

    fn compile_object_rest_binding(
        &mut self,
        rest_pattern: &BindingPattern<'_>,
        source_register: BytecodeRegister,
        excluded_keys: &[BytecodeRegister],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let rest_object = self.allocate_local()?;
        self.instructions.push(Instruction::new_object(rest_object));

        let excluded_array = self.allocate_local()?;
        self.instructions
            .push(Instruction::new_array(excluded_array, 0));
        for (index, excluded) in excluded_keys.iter().enumerate() {
            let index_value = self
                .load_i32(i32::try_from(index).map_err(|_| SourceLoweringError::TooManyLocals)?)?;
            self.instructions.push(Instruction::set_index(
                excluded_array,
                index_value.register,
                *excluded,
            ));
            self.release(index_value);
        }
        self.instructions
            .push(Instruction::copy_data_properties_except(
                rest_object,
                source_register,
                excluded_array,
            ));

        self.compile_binding_pattern_target(
            rest_pattern,
            ValueLocation::local(rest_object),
            false,
            module,
        )
    }

    fn compile_object_rest_assignment_target(
        &mut self,
        rest_target: &AssignmentTarget<'_>,
        source_register: BytecodeRegister,
        excluded_keys: &[BytecodeRegister],
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let rest_object = self.allocate_local()?;
        self.instructions.push(Instruction::new_object(rest_object));

        let excluded_array = self.allocate_local()?;
        self.instructions
            .push(Instruction::new_array(excluded_array, 0));
        for (index, excluded) in excluded_keys.iter().enumerate() {
            let index_value = self
                .load_i32(i32::try_from(index).map_err(|_| SourceLoweringError::TooManyLocals)?)?;
            self.instructions.push(Instruction::set_index(
                excluded_array,
                index_value.register,
                *excluded,
            ));
            self.release(index_value);
        }
        self.instructions
            .push(Instruction::copy_data_properties_except(
                rest_object,
                source_register,
                excluded_array,
            ));

        self.compile_assignment_target(rest_target, ValueLocation::local(rest_object), module)
    }

    /// Destructure an object pattern `{ a, b: c, d = default }` from `source_register`.
    /// §14.3.3.2 — RequireObjectCoercible(value) before destructuring.
    /// Spec: <https://tc39.es/ecma262/#sec-destructuring-binding-patterns-runtime-semantics-bindinginitialization>
    pub(super) fn compile_object_destructuring(
        &mut self,
        pattern: &oxc_ast::ast::ObjectPattern<'_>,
        source_register: BytecodeRegister,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        // §14.3.3.2 step 1: RequireObjectCoercible(value).
        // Throws TypeError if value is null or undefined. For non-empty
        // patterns this happens naturally during property access, but empty
        // patterns `{} = null` need an explicit check.
        // Spec: <https://tc39.es/ecma262/#sec-requireobjectcoercible>
        if pattern.properties.is_empty() && pattern.rest.is_none() {
            let tmp = self.alloc_temp();
            let prop = self.intern_property_name("constructor")?;
            self.instructions
                .push(Instruction::get_property(tmp, source_register, prop));
            self.release(ValueLocation::temp(tmp));
        }
        let has_rest = pattern.rest.is_some();
        let mut excluded_keys = Vec::with_capacity(pattern.properties.len());
        for prop in &pattern.properties {
            let key = self.compile_object_property_key_value(prop, module)?;
            let key_register = if has_rest {
                let key_loc = self.stabilize_binding_value(key)?;
                excluded_keys.push(key_loc.register);
                key_loc.register
            } else {
                key.register
            };

            let extracted = self.alloc_temp();
            self.instructions.push(Instruction::get_index(
                extracted,
                source_register,
                key_register,
            ));
            if !has_rest {
                self.release(key);
            }
            let extracted_loc = self.stabilize_binding_value(ValueLocation::temp(extracted))?;

            self.compile_binding_pattern_target(
                &prop.value,
                extracted_loc,
                prop.shorthand,
                module,
            )?;
        }

        if let Some(rest) = &pattern.rest {
            self.compile_object_rest_binding(
                &rest.argument,
                source_register,
                &excluded_keys,
                module,
            )?;
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
        let iterator_register = self.allocate_local()?;
        let done_register = self.allocate_local()?;
        let step_value_register = self.allocate_local()?;
        self.instructions.push(Instruction::get_iterator(
            iterator_register,
            source_register,
        ));

        let try_start = self.instructions.len();
        for element in &pattern.elements {
            if element.is_none() {
                self.instructions.push(Instruction::iterator_next(
                    done_register,
                    step_value_register,
                    iterator_register,
                ));
                continue;
            }

            let extracted_register = self.allocate_local()?;
            self.emit_iterator_value_or_undefined(
                iterator_register,
                done_register,
                step_value_register,
                extracted_register,
            )?;
            self.compile_binding_pattern_target(
                element.as_ref().expect("checked Some above"),
                ValueLocation::local(extracted_register),
                false,
                module,
            )?;
        }

        if let Some(rest) = &pattern.rest {
            let rest_array = self.allocate_local()?;
            self.instructions
                .push(Instruction::new_array(rest_array, 0));
            let rest_index = self.allocate_local()?;
            self.instructions.push(Instruction::load_i32(rest_index, 0));
            let one = self.allocate_local()?;
            self.instructions.push(Instruction::load_i32(one, 1));

            let loop_start = self.instructions.len();
            self.instructions.push(Instruction::iterator_next(
                done_register,
                step_value_register,
                iterator_register,
            ));
            let exit_jump = self.emit_conditional_placeholder(Opcode::JumpIfTrue, done_register);
            self.instructions.push(Instruction::set_index(
                rest_array,
                rest_index,
                step_value_register,
            ));
            let next_rest_index = self.alloc_temp();
            self.instructions
                .push(Instruction::add(next_rest_index, rest_index, one));
            self.instructions
                .push(Instruction::move_(rest_index, next_rest_index));
            self.release(ValueLocation::temp(next_rest_index));
            self.emit_relative_jump(loop_start)?;
            self.patch_jump(exit_jump, self.instructions.len())?;

            self.compile_binding_pattern_target(
                &rest.argument,
                ValueLocation::local(rest_array),
                false,
                module,
            )?;
        }

        self.instructions
            .push(Instruction::iterator_close(iterator_register));
        let try_end = self.instructions.len();
        self.emit_destructuring_iterator_handler(try_start, try_end, iterator_register)?;
        Ok(())
    }

    pub(super) fn compile_object_assignment_destructuring(
        &mut self,
        pattern: &oxc_ast::ast::ObjectAssignmentTarget<'_>,
        source_register: BytecodeRegister,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let has_rest = pattern.rest.is_some();
        let mut excluded_keys = Vec::with_capacity(pattern.properties.len());
        for prop in &pattern.properties {
            match prop {
                AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(identifier) => {
                    let key = self.compile_string_literal(identifier.binding.name.as_str())?;
                    let key_register = if has_rest {
                        let key_loc = self.stabilize_binding_value(key)?;
                        excluded_keys.push(key_loc.register);
                        key_loc.register
                    } else {
                        key.register
                    };
                    let extracted = self.alloc_temp();
                    self.instructions.push(Instruction::get_index(
                        extracted,
                        source_register,
                        key_register,
                    ));
                    if !has_rest {
                        self.release(key);
                    }
                    let extracted_loc =
                        self.stabilize_binding_value(ValueLocation::temp(extracted))?;
                    if let Some(init) = &identifier.init {
                        let undef = self.load_undefined()?;
                        let cmp = ValueLocation::temp(self.alloc_temp());
                        self.instructions.push(Instruction::eq(
                            cmp.register,
                            extracted_loc.register,
                            undef.register,
                        ));
                        self.release(undef);
                        let jump_skip =
                            self.emit_conditional_placeholder(Opcode::JumpIfFalse, cmp.register);
                        self.release(cmp);
                        let default_val = self.compile_expression_with_inferred_name(
                            init,
                            Some(identifier.binding.name.as_str()),
                            module,
                        )?;
                        if default_val.register != extracted_loc.register {
                            self.instructions.push(Instruction::move_(
                                extracted_loc.register,
                                default_val.register,
                            ));
                            self.release(default_val);
                        }
                        self.patch_jump(jump_skip, self.instructions.len())?;
                    }
                    let _ = self.assign_to_name(identifier.binding.name.as_str(), extracted_loc)?;
                }
                AssignmentTargetProperty::AssignmentTargetPropertyProperty(property) => {
                    let key = self.compile_assignment_property_key_value(property, module)?;
                    let key_register = if has_rest {
                        let key_loc = self.stabilize_binding_value(key)?;
                        excluded_keys.push(key_loc.register);
                        key_loc.register
                    } else {
                        key.register
                    };
                    let extracted = self.alloc_temp();
                    self.instructions.push(Instruction::get_index(
                        extracted,
                        source_register,
                        key_register,
                    ));
                    if !has_rest {
                        self.release(key);
                    }
                    let extracted_loc =
                        self.stabilize_binding_value(ValueLocation::temp(extracted))?;
                    self.compile_assignment_target_maybe_default(
                        &property.binding,
                        extracted_loc,
                        module,
                    )?;
                }
            }
        }

        if let Some(rest) = &pattern.rest {
            self.compile_object_rest_assignment_target(
                &rest.target,
                source_register,
                &excluded_keys,
                module,
            )?;
        }

        Ok(())
    }

    pub(super) fn compile_array_assignment_destructuring(
        &mut self,
        pattern: &oxc_ast::ast::ArrayAssignmentTarget<'_>,
        source_register: BytecodeRegister,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        let iterator_register = self.allocate_local()?;
        let done_register = self.allocate_local()?;
        let step_value_register = self.allocate_local()?;
        self.instructions.push(Instruction::get_iterator(
            iterator_register,
            source_register,
        ));

        let try_start = self.instructions.len();
        for element in &pattern.elements {
            if element.is_none() {
                self.instructions.push(Instruction::iterator_next(
                    done_register,
                    step_value_register,
                    iterator_register,
                ));
                continue;
            }

            let extracted_register = self.allocate_local()?;
            self.emit_iterator_value_or_undefined(
                iterator_register,
                done_register,
                step_value_register,
                extracted_register,
            )?;
            self.compile_assignment_target_maybe_default(
                element.as_ref().expect("checked Some above"),
                ValueLocation::local(extracted_register),
                module,
            )?;
        }

        if let Some(rest) = &pattern.rest {
            let rest_array = self.allocate_local()?;
            self.instructions
                .push(Instruction::new_array(rest_array, 0));
            let rest_index = self.allocate_local()?;
            self.instructions.push(Instruction::load_i32(rest_index, 0));
            let one = self.allocate_local()?;
            self.instructions.push(Instruction::load_i32(one, 1));

            let loop_start = self.instructions.len();
            self.instructions.push(Instruction::iterator_next(
                done_register,
                step_value_register,
                iterator_register,
            ));
            let exit_jump = self.emit_conditional_placeholder(Opcode::JumpIfTrue, done_register);
            self.instructions.push(Instruction::set_index(
                rest_array,
                rest_index,
                step_value_register,
            ));
            let next_rest_index = self.alloc_temp();
            self.instructions
                .push(Instruction::add(next_rest_index, rest_index, one));
            self.instructions
                .push(Instruction::move_(rest_index, next_rest_index));
            self.release(ValueLocation::temp(next_rest_index));
            self.emit_relative_jump(loop_start)?;
            self.patch_jump(exit_jump, self.instructions.len())?;

            self.compile_assignment_target(&rest.target, ValueLocation::local(rest_array), module)?;
        }

        self.instructions
            .push(Instruction::iterator_close(iterator_register));
        let try_end = self.instructions.len();
        self.emit_destructuring_iterator_handler(try_start, try_end, iterator_register)?;
        Ok(())
    }

    pub(super) fn compile_assignment_target_maybe_default(
        &mut self,
        target: &AssignmentTargetMaybeDefault<'_>,
        value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        match target {
            AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(assignment) => {
                let materialized = self.stabilize_binding_value(value)?;
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
                let default_val = self.compile_expression_with_inferred_name(
                    &assignment.init,
                    super::ast::inferred_name_for_assignment_target(&assignment.binding),
                    module,
                )?;
                if default_val.register != materialized.register {
                    self.instructions.push(Instruction::move_(
                        materialized.register,
                        default_val.register,
                    ));
                    self.release(default_val);
                }
                self.patch_jump(jump_skip, self.instructions.len())?;
                self.compile_assignment_target(&assignment.binding, materialized, module)
            }
            AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(identifier) => {
                let _ = self.assign_to_name(identifier.name.as_str(), value)?;
                Ok(())
            }
            AssignmentTargetMaybeDefault::ComputedMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                self.store_computed_member(object, module, member, value)
            }
            AssignmentTargetMaybeDefault::StaticMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.instructions.push(Instruction::set_property(
                    object.register,
                    value.register,
                    property,
                ));
                self.release(object);
                Ok(())
            }
            AssignmentTargetMaybeDefault::ArrayAssignmentTarget(pattern) => {
                let materialized = self.stabilize_binding_value(value)?;
                self.compile_array_assignment_destructuring(pattern, materialized.register, module)
            }
            AssignmentTargetMaybeDefault::ObjectAssignmentTarget(pattern) => {
                let materialized = self.stabilize_binding_value(value)?;
                self.compile_object_assignment_destructuring(pattern, materialized.register, module)
            }
            _ => Err(SourceLoweringError::Unsupported(
                "assignment target maybe default".to_string(),
            )),
        }
    }

    pub(super) fn compile_assignment_target(
        &mut self,
        target: &AssignmentTarget<'_>,
        value: ValueLocation,
        module: &mut ModuleCompiler<'a>,
    ) -> Result<(), SourceLoweringError> {
        match target {
            AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
                let _ = self.assign_to_name(identifier.name.as_str(), value)?;
                Ok(())
            }
            AssignmentTarget::StaticMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                let property = self.intern_property_name(member.property.name.as_str())?;
                self.instructions.push(Instruction::set_property(
                    object.register,
                    value.register,
                    property,
                ));
                self.release(object);
                Ok(())
            }
            AssignmentTarget::ComputedMemberExpression(member) => {
                let object = self.compile_expression(&member.object, module)?;
                self.store_computed_member(object, module, member, value)
            }
            AssignmentTarget::ObjectAssignmentTarget(object_pattern) => {
                let materialized = self.stabilize_binding_value(value)?;
                self.compile_object_assignment_destructuring(
                    object_pattern,
                    materialized.register,
                    module,
                )?;
                Ok(())
            }
            AssignmentTarget::ArrayAssignmentTarget(array_pattern) => {
                let materialized = self.stabilize_binding_value(value)?;
                self.compile_array_assignment_destructuring(
                    array_pattern,
                    materialized.register,
                    module,
                )?;
                Ok(())
            }
            _ => Err(SourceLoweringError::Unsupported(
                "assignment target".to_string(),
            )),
        }
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
                        Binding::ThisRegister(_)
                        | Binding::ThisUpvalue(_)
                        | Binding::ImmutableRegister(_)
                        | Binding::ImmutableUpvalue(_) => {
                            return Err(SourceLoweringError::Unsupported(
                                "destructuring cannot bind the lexical this binding".to_string(),
                            ));
                        }
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
                let materialized = self.stabilize_binding_value(value)?;
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
                let default_val = self.compile_expression_with_inferred_name(
                    &assignment.right,
                    super::ast::inferred_name_for_binding_pattern(&assignment.left),
                    module,
                )?;
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
                let materialized = self.stabilize_binding_value(value)?;
                self.compile_object_destructuring(object_pattern, materialized.register, module)?;
                self.release(materialized);
                Ok(())
            }
            BindingPattern::ArrayPattern(array_pattern) => {
                let materialized = self.stabilize_binding_value(value)?;
                self.compile_array_destructuring(array_pattern, materialized.register, module)?;
                self.release(materialized);
                Ok(())
            }
        }
    }
}
