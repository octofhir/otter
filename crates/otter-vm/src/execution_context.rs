//! Isolate-local execution context handles.
//!
//! A queued JS job must know which compiled code table owns its
//! callback. Production engines get that from their internal
//! function / realm / module records; Otter keeps the same boundary
//! explicit with this small handle.
//!
//! # Contents
//!
//! - [`ExecutionContext`] — cloneable VM-owned dispatch context.
//!
//! # Invariants
//!
//! - Host schedulers never receive an [`ExecutionContext`]. They
//!   only see opaque tokens and wake the isolate.
//! - JS-visible work queued inside the VM carries its context with
//!   the job, so dispatch never depends on ambient runtime state.
//! - The bytecode module is an implementation detail of the
//!   context. Callers use narrow accessors for function-table,
//!   constant-pool, and module-resolution reads.
//!
//! # See also
//!
//! - [`crate::microtask`]
//! - [`crate::timers`]
//! - [`crate::dynamic_import`]

use otter_bytecode::{BytecodeModule, Constant, Function, ModuleInit, Operand};

use crate::executable::{ExecInstr, ExecutableFunction, ExecutableModule};

/// Cloneable dispatch context for VM-owned JS jobs.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    module: std::rc::Rc<BytecodeModule>,
    executable: std::rc::Rc<ExecutableModule>,
    decoded_strings: std::rc::Rc<[Option<String>]>,
}

impl ExecutionContext {
    /// Build a context from an owned bytecode module.
    #[must_use]
    pub fn from_module(module: BytecodeModule) -> Self {
        let executable = ExecutableModule::from_bytecode(&module);
        let decoded_strings: std::rc::Rc<[Option<String>]> = module
            .constants
            .iter()
            .map(|constant| match constant {
                Constant::String { utf16 } => Some(String::from_utf16_lossy(utf16)),
                _ => None,
            })
            .collect();
        Self {
            module: std::rc::Rc::new(module),
            executable: std::rc::Rc::new(executable),
            decoded_strings,
        }
    }

    /// Synthetic bytecode module name.
    #[must_use]
    pub fn module_name(&self) -> &str {
        &self.module.module
    }

    /// Entry function for a script/module turn.
    #[must_use]
    pub fn main(&self) -> &Function {
        self.module.main()
    }

    /// Entry executable function for a script/module turn.
    #[must_use]
    pub(crate) fn exec_main(&self) -> &ExecutableFunction {
        self.executable
            .function(0)
            .expect("bytecode modules always carry main function 0")
    }

    /// Module initialization records for linked module graphs.
    #[must_use]
    pub fn module_inits(&self) -> &[ModuleInit] {
        &self.module.module_inits
    }

    /// Function-table lookup by VM function id.
    #[must_use]
    pub fn function(&self, function_id: u32) -> Option<&Function> {
        self.module.functions.get(function_id as usize)
    }

    /// Executable function lookup by VM function id.
    #[must_use]
    pub(crate) fn exec_function(&self, function_id: u32) -> Option<&ExecutableFunction> {
        self.executable.function(function_id)
    }

    /// Return an executable instruction's operands in declaration order.
    #[must_use]
    pub(crate) fn exec_operands<'a>(&'a self, instr: &'a ExecInstr) -> &'a [Operand] {
        self.executable.operands(instr)
    }

    /// Return one executable instruction operand by index.
    #[must_use]
    pub(crate) fn exec_operand<'a>(
        &'a self,
        instr: &'a ExecInstr,
        index: usize,
    ) -> Option<&'a Operand> {
        self.executable.operand(instr, index)
    }

    /// Decode one executable register operand.
    #[must_use]
    pub(crate) fn exec_register(&self, instr: &ExecInstr, index: usize) -> Option<u16> {
        self.executable.register(instr, index)
    }

    /// Decode the common `dst, lhs, rhs` register triple.
    #[must_use]
    pub(crate) fn exec_register3(&self, instr: &ExecInstr) -> Option<(u16, u16, u16)> {
        Some((
            self.exec_register(instr, 0)?,
            self.exec_register(instr, 1)?,
            self.exec_register(instr, 2)?,
        ))
    }

    /// Decode one executable constant-pool index operand.
    #[must_use]
    pub(crate) fn exec_const_index(&self, instr: &ExecInstr, index: usize) -> Option<u32> {
        self.executable.const_index(instr, index)
    }

    /// Decode one executable signed immediate operand.
    #[must_use]
    pub(crate) fn exec_imm32(&self, instr: &ExecInstr, index: usize) -> Option<i32> {
        self.executable.imm32(instr, index)
    }

    /// `true` when the function id points at an arrow function.
    #[must_use]
    pub fn function_is_arrow(&self, function_id: u32) -> bool {
        self.exec_function(function_id).is_some_and(|f| f.is_arrow)
    }

    /// `true` when the function id points at a strict function.
    #[must_use]
    pub fn function_is_strict(&self, function_id: u32) -> bool {
        self.exec_function(function_id).is_some_and(|f| f.is_strict)
    }

    /// Resolve a function-id constant.
    #[must_use]
    pub fn function_id_constant(&self, idx: u32) -> Option<u32> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::FunctionId { index }) => Some(*index),
            _ => None,
        }
    }

    /// Resolve a string constant as WTF-16 code units.
    #[must_use]
    pub fn string_constant_units(&self, idx: u32) -> Option<&[u16]> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::String { utf16 }) => Some(utf16.as_slice()),
            _ => None,
        }
    }

    /// Resolve a string constant as a borrowed UTF-8 string.
    #[must_use]
    pub fn string_constant_str(&self, idx: u32) -> Option<&str> {
        self.decoded_strings
            .get(idx as usize)
            .and_then(Option::as_deref)
    }

    /// Resolve a numeric constant's raw IEEE-754 bits.
    #[must_use]
    pub fn number_constant_bits(&self, idx: u32) -> Option<u64> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::Number { bits }) => Some(*bits),
            _ => None,
        }
    }

    /// Resolve a BigInt decimal literal constant.
    #[must_use]
    pub fn bigint_decimal_constant(&self, idx: u32) -> Option<&str> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::BigInt { decimal }) => Some(decimal.as_str()),
            _ => None,
        }
    }

    /// Resolve a RegExp literal constant.
    #[must_use]
    pub fn regexp_constant(&self, idx: u32) -> Option<(&[u16], &str)> {
        match self.module.constants.get(idx as usize) {
            Some(Constant::RegExp {
                pattern_utf16,
                flags,
            }) => Some((pattern_utf16.as_slice(), flags.as_str())),
            _ => None,
        }
    }

    /// Resolve a module import edge from the bytecode resolution table.
    #[must_use]
    pub fn module_resolution_target(&self, referrer: &str, specifier: &str) -> Option<&str> {
        self.module
            .module_resolutions
            .iter()
            .find(|r| r.referrer == referrer && r.specifier == specifier)
            .map(|r| r.target.as_str())
    }
}
