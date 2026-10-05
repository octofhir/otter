//! Site-decoded committed binding and global-declaration calls.
//!
//! # Contents
//! - [`RuntimeCall::binding_values`] completes the schema-owned binding family.
//! - [`RuntimeCall::global_declaration_values`] completes the separate global
//!   declaration/initialization family.
//!
//! # Invariants
//! - The physical or inline source function/PC and `OpcodeSchema` are the sole
//!   semantic authority;
//!   the native ABI carries only two boxed SSA values, in
//!   `BindingSemantics::value_operands` order: the stored value first, then
//!   the context or reference register (or the pre-RHS existence Boolean).
//! - `Lookup*` operations decode one schema-owned static hop bound; an
//!   extension probe cannot cross the declaring context.
//! - Structural site, constant, immediate, and context failures are fatal.
//!   Entered JavaScript semantics return only success or a catchable error.
//! - Both boxed inputs and the result are rooted for the complete allocating or
//!   reentrant operation; allocating kernels re-read contexts from those roots.
//! - Source and immutable operands are resolved before exclusive VM access.
//!   GlobalThis and global binding operations use that source FID's linked
//!   realm even when the published caller currently runs in another realm.
//! - No operation advances the PC or writes a destination register.
//!
//! # See also
//! - `otter_bytecode::opcode_schema::BindingSemantics`
//! - [`super::CommittedValueError`]

use otter_bytecode::opcode_schema::{
    BindingDelete, BindingMissing, BindingRead, BindingSemantics, BindingWrite,
    GlobalDeclarationSemantics, opcode_schema,
};

use crate::{Value, VmError, rooting::RootScopeExt};

use super::{CommittedValueError, RuntimeCall};

fn flag(value: i32) -> Result<bool, CommittedValueError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CommittedValueError::Fatal(VmError::InvalidOperand)),
    }
}

impl RuntimeCall<'_> {
    /// Complete the exact schema-typed binding site from boxed SSA inputs.
    pub fn binding_values(
        &mut self,
        mut value0: Value,
        mut value1: Value,
    ) -> Result<Value, CommittedValueError> {
        let (function_id, instruction_pc) =
            self.semantic_source().map_err(CommittedValueError::Fatal)?;
        let owner = self
            .context
            .for_function(function_id)
            .map_err(|_| CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let context = &*owner;
        let function = context
            .exec_function(function_id)
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let instruction = function
            .instr_at_index(instruction_pc as usize)
            .filter(|instruction| instruction.instruction_pc == instruction_pc)
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let operation = opcode_schema(function.op(instruction))
            .binding
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let const_index = |operand: u8| {
            function
                .const_index(instruction, usize::from(operand))
                .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
        };
        let imm32 = |operand: u8| {
            function
                .imm32(instruction, usize::from(operand))
                .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
        };
        // Source and immutable operands are resolved before exclusive VM access.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let stack = unsafe { &mut *self.stack.as_ptr() };

        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut vm.gc_heap);
        // SAFETY: all three locals precede `roots`, remain stationary, and do
        // not escape the complete committed call.
        unsafe {
            roots.add_value(&mut value0);
            roots.add_value(&mut value1);
            roots.add_value(&mut result);
        }

        result = match operation {
            BindingSemantics::Read(BindingRead::GlobalThis { .. }) => vm
                .global_this_for_function(function_id)
                .map(Value::object)
                .map_err(CommittedValueError::Fatal),
            BindingSemantics::Read(BindingRead::Global { name, missing, .. }) => {
                let name_idx = const_index(name)?;
                match missing {
                    BindingMissing::Throw => {
                        vm.load_global_or_throw_value(stack, context, function_id, name_idx)
                    }
                    BindingMissing::Undefined => {
                        vm.load_global_or_undefined_value(context, stack, function_id, name_idx)
                    }
                }
            }
            BindingSemantics::Read(BindingRead::Exists { name, .. }) => {
                let name_idx = const_index(name)?;
                vm.global_binding_exists_value(stack, context, function_id, name_idx)
            }
            BindingSemantics::Read(BindingRead::ContextSlot { coord, .. }) => {
                let coord = imm32(coord)?;
                vm.load_context_slot_value(context, value0, coord, true)
                    .map_err(CommittedValueError::JavaScript)
            }
            BindingSemantics::Read(BindingRead::LookupSlot { name, coord, .. }) => {
                let name_idx = const_index(name)?;
                let coord = imm32(coord)?;
                vm.load_lookup_slot_value(context, function_id, value0, name_idx, coord)
                    .map_err(CommittedValueError::JavaScript)
            }
            BindingSemantics::Read(BindingRead::LookupGlobal {
                name,
                depth,
                missing,
                ..
            }) => {
                let name_idx = const_index(name)?;
                let depth = imm32(depth)?;
                vm.load_lookup_global_value(
                    context,
                    stack,
                    function_id,
                    value0,
                    name_idx,
                    depth,
                    missing,
                )
            }
            BindingSemantics::Read(BindingRead::ResolveRef { name, target, .. }) => {
                let name_idx = const_index(name)?;
                let target = imm32(target)?;
                vm.resolve_lookup_ref_value(context, function_id, value0, name_idx, target)
                    .map_err(CommittedValueError::JavaScript)
            }
            BindingSemantics::Write(BindingWrite::Global { name, strict, .. }) => {
                let name_idx = const_index(name)?;
                let strict = flag(imm32(strict)?)?;
                vm.store_global_binding_value(context, stack, function_id, value0, name_idx, strict)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::GlobalChecked { name, .. }) => {
                let name_idx = const_index(name)?;
                let existed = value1
                    .as_boolean()
                    .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
                vm.store_global_checked_value(
                    context,
                    stack,
                    function_id,
                    value0,
                    name_idx,
                    existed,
                )
                .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::ContextSlot { coord, .. }) => {
                let coord = imm32(coord)?;
                vm.store_context_slot_value(context, value1, coord, value0, true)
                    .map_err(CommittedValueError::JavaScript)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::BindThis { coord, .. }) => {
                let coord = imm32(coord)?;
                vm.bind_this_context_slot_value(value1, coord, value0)
                    .map_err(CommittedValueError::JavaScript)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::LookupSlot {
                name,
                coord,
                fallback,
                ..
            }) => {
                let name_idx = const_index(name)?;
                let coord = imm32(coord)?;
                let fallback = imm32(fallback)?;
                vm.store_lookup_slot_value(
                    context,
                    function_id,
                    value1,
                    name_idx,
                    coord,
                    fallback,
                    value0,
                )
                .map_err(CommittedValueError::JavaScript)
                .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::LookupGlobal { name, mode, .. }) => {
                let name_idx = const_index(name)?;
                let mode = imm32(mode)?;
                vm.store_lookup_global_value(
                    context,
                    stack,
                    function_id,
                    value1,
                    name_idx,
                    mode,
                    value0,
                )
                .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::StoreRef { name, mode, .. }) => {
                let name_idx = const_index(name)?;
                let mode = imm32(mode)?;
                vm.store_ref_value(context, stack, function_id, value1, name_idx, mode, value0)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::DeclareEvalVar {
                name, var_depth, ..
            }) => {
                let name_idx = const_index(name)?;
                let var_depth = imm32(var_depth)?;
                vm.declare_eval_var_value(context, function_id, value0, name_idx, var_depth)
                    .map_err(CommittedValueError::JavaScript)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Write(BindingWrite::VarScope {
                name, var_depth, ..
            }) => {
                let name_idx = const_index(name)?;
                let var_depth = imm32(var_depth)?;
                vm.store_var_scope_value(context, function_id, value1, name_idx, var_depth, value0)
                    .map_err(CommittedValueError::JavaScript)
                    .map(|()| Value::undefined())
            }
            BindingSemantics::Delete(BindingDelete::LookupSlot { name, depth, .. }) => {
                let name_idx = const_index(name)?;
                let depth = imm32(depth)?;
                vm.delete_lookup_slot_value(context, function_id, value0, name_idx, depth)
                    .map_err(CommittedValueError::JavaScript)
            }
            BindingSemantics::Delete(BindingDelete::LookupGlobal { name, depth, .. }) => {
                let name_idx = const_index(name)?;
                let depth = imm32(depth)?;
                vm.delete_lookup_global_value(context, function_id, value0, name_idx, depth)
                    .map_err(CommittedValueError::JavaScript)
            }
        }?;
        Ok(result)
    }

    /// Complete the exact schema-typed global declaration site.
    pub fn global_declaration_values(
        &mut self,
        mut value0: Value,
        mut value1: Value,
    ) -> Result<Value, CommittedValueError> {
        let (function_id, instruction_pc) =
            self.semantic_source().map_err(CommittedValueError::Fatal)?;
        let owner = self
            .context
            .for_function(function_id)
            .map_err(|_| CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let context = &*owner;
        let function = context
            .exec_function(function_id)
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let instruction = function
            .instr_at_index(instruction_pc as usize)
            .filter(|instruction| instruction.instruction_pc == instruction_pc)
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let operation = opcode_schema(function.op(instruction))
            .global_declaration
            .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))?;
        let const_index = |operand: u8| {
            function
                .const_index(instruction, usize::from(operand))
                .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
        };
        let imm32 = |operand: u8| {
            function
                .imm32(instruction, usize::from(operand))
                .ok_or(CommittedValueError::Fatal(VmError::InvalidOperand))
        };
        // Source and immutable operands are resolved before exclusive VM access.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        vm.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        // SAFETY: RuntimeCall holds exclusive ownership of this current
        // activation stack for the complete committed operation.
        let stack = unsafe { &mut *self.stack.as_ptr() };

        let mut result = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut vm.gc_heap);
        // SAFETY: the stationary boxed locals remain within this call.
        unsafe {
            roots.add_value(&mut value0);
            roots.add_value(&mut value1);
            roots.add_value(&mut result);
        }

        let semantic = match operation {
            GlobalDeclarationSemantics::DeclareVar { name, configurable } => {
                let name_idx = const_index(name)?;
                let configurable = flag(imm32(configurable)?)?;
                vm.declare_global_var_value(context, function_id, name_idx, configurable)
                    .map_err(CommittedValueError::JavaScript)
            }
            GlobalDeclarationSemantics::DeclareLexical { name, is_const } => {
                let name_idx = const_index(name)?;
                let is_const = flag(imm32(is_const)?)?;
                vm.declare_global_lex_value(context, function_id, name_idx, is_const)
                    .map_err(CommittedValueError::JavaScript)
            }
            GlobalDeclarationSemantics::Validate {
                name,
                declaration_kind,
            } => {
                let name_idx = const_index(name)?;
                let kind = imm32(declaration_kind)?;
                if !(0..=2).contains(&kind) {
                    return Err(CommittedValueError::Fatal(VmError::InvalidOperand));
                }
                vm.validate_global_decl_value(context, function_id, name_idx, kind)
                    .map_err(CommittedValueError::JavaScript)
            }
            GlobalDeclarationSemantics::DefineVar { name, .. } => {
                let name_idx = const_index(name)?;
                vm.define_global_var_value(context, stack, function_id, name_idx, value0)
            }
            GlobalDeclarationSemantics::DefineFunction {
                name, deletable, ..
            } => {
                let name_idx = const_index(name)?;
                let deletable = flag(imm32(deletable)?)?;
                vm.define_global_function_value(context, function_id, name_idx, value0, deletable)
                    .map_err(CommittedValueError::JavaScript)
            }
            GlobalDeclarationSemantics::InitializeLexical { name, .. } => {
                let name_idx = const_index(name)?;
                vm.init_global_lex_value(context, function_id, name_idx, value0)
                    .map_err(CommittedValueError::JavaScript)
            }
        };
        semantic?;
        Ok(result)
    }
}
