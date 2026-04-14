//! Single call frame: register file, program counter, pending exception,
//! overflow-arguments spill area, open-upvalue bookkeeping, and receiver /
//! new-target plumbing. One `Activation` per active function call.

use crate::bytecode::{BytecodeRegister, Instruction, ProgramCounter};
use crate::frame::{FrameMetadata, RegisterIndex};
use crate::module::{Function, FunctionIndex};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::InterpreterError;
use super::RuntimeState;

/// Mutable activation state for a single executing function frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    function_index: FunctionIndex,
    pub(super) metadata: FrameMetadata,
    closure_handle: Option<ObjectHandle>,
    construct_new_target: Option<ObjectHandle>,
    pending_exception: Option<RegisterValue>,
    pc: ProgramCounter,
    registers: Box<[RegisterValue]>,
    open_upvalues: Box<[Option<ObjectHandle>]>,
    written_registers: Vec<RegisterIndex>,
    /// ES2024 §10.4.4 — Overflow arguments beyond formal parameter count.
    /// Stored separately from the register file to avoid polluting the frame layout.
    /// Used by `CreateArguments` to populate the arguments exotic object.
    pub(super) overflow_args: Vec<RegisterValue>,
    /// V8 Ignition-style implicit accumulator for the v2 bytecode
    /// dispatch. Holds the transient expression value that most v2
    /// arithmetic / comparison / property ops read and write. Unused by
    /// the v1 dispatch loop, but kept on every `Activation` so that
    /// generator save/resume + deopt materialization remain uniform
    /// across both ISAs.
    ///
    /// Defaults to `undefined` on frame creation. Preserved as part of
    /// `save_registers` / `restore_registers` so yield/await round-trip
    /// the accumulator through the generator continuation.
    pub(super) accumulator: RegisterValue,
}

impl Activation {
    /// Creates a zero-initialized activation for the given function.
    #[must_use]
    pub fn new(function_index: FunctionIndex, register_count: RegisterIndex) -> Self {
        Self::with_metadata(function_index, register_count, FrameMetadata::default())
    }

    /// Creates a zero-initialized activation with explicit frame metadata.
    #[must_use]
    pub fn with_metadata(
        function_index: FunctionIndex,
        register_count: RegisterIndex,
        metadata: FrameMetadata,
    ) -> Self {
        Self::with_context(function_index, register_count, metadata, None)
    }

    /// Creates a zero-initialized activation with explicit frame metadata and closure context.
    #[must_use]
    pub fn with_context(
        function_index: FunctionIndex,
        register_count: RegisterIndex,
        metadata: FrameMetadata,
        closure_handle: Option<ObjectHandle>,
    ) -> Self {
        Self {
            function_index,
            metadata,
            closure_handle,
            construct_new_target: None,
            pending_exception: None,
            pc: 0,
            registers: vec![RegisterValue::default(); usize::from(register_count)]
                .into_boxed_slice(),
            open_upvalues: vec![None; usize::from(register_count)].into_boxed_slice(),
            written_registers: Vec::new(),
            overflow_args: Vec::new(),
            accumulator: RegisterValue::undefined(),
        }
    }

    /// Returns the current function index.
    #[must_use]
    pub const fn function_index(&self) -> FunctionIndex {
        self.function_index
    }

    /// Returns the frame metadata for the activation.
    #[must_use]
    pub const fn metadata(&self) -> FrameMetadata {
        self.metadata
    }

    /// Returns the current closure context, if one exists.
    #[must_use]
    pub const fn closure_handle(&self) -> Option<ObjectHandle> {
        self.closure_handle
    }

    #[must_use]
    pub const fn construct_new_target(&self) -> Option<ObjectHandle> {
        self.construct_new_target
    }

    pub fn set_construct_new_target(&mut self, new_target: Option<ObjectHandle>) {
        self.construct_new_target = new_target;
    }

    /// Returns the pending exception value, if one exists.
    #[must_use]
    pub const fn pending_exception(&self) -> Option<RegisterValue> {
        self.pending_exception
    }

    /// Returns the current program counter.
    #[must_use]
    pub const fn pc(&self) -> ProgramCounter {
        self.pc
    }

    /// Overwrites the current program counter explicitly.
    pub fn set_pc(&mut self, pc: ProgramCounter) {
        self.pc = pc;
    }

    pub(super) fn set_pending_exception(&mut self, value: RegisterValue) {
        self.pending_exception = Some(value);
    }

    pub(super) fn take_pending_exception(&mut self) -> Option<RegisterValue> {
        self.pending_exception.take()
    }

    /// Returns the immutable register slice.
    #[must_use]
    pub fn registers(&self) -> &[RegisterValue] {
        &self.registers
    }

    /// Reads the v2 accumulator. Undefined for frames running v1
    /// bytecode; only the v2 dispatcher writes it.
    #[must_use]
    pub fn accumulator(&self) -> RegisterValue {
        self.accumulator
    }

    /// Overwrites the v2 accumulator.
    pub fn set_accumulator(&mut self, value: RegisterValue) {
        self.accumulator = value;
    }

    /// Returns a raw mutable pointer to the register file for JIT native
    /// execution. The pointer is valid as long as the `Activation` is not
    /// moved or dropped.
    ///
    /// # Safety
    ///
    /// Callers must ensure that the register file is not aliased by any
    /// other reference while the pointer is live and must respect the
    /// `register_count()` bound.
    #[must_use]
    pub fn registers_mut_ptr(&mut self) -> *mut RegisterValue {
        self.registers.as_mut_ptr()
    }

    /// Returns the total number of register slots in the frame.
    #[must_use]
    pub fn register_count(&self) -> usize {
        self.registers.len()
    }

    /// Saves the entire register window as a boxed slice for generator suspension.
    pub fn save_registers(&self) -> Box<[RegisterValue]> {
        self.registers.clone()
    }

    /// Restores a previously saved register window into this activation.
    pub fn restore_registers(&mut self, saved: &[RegisterValue]) {
        let copy_len = saved.len().min(self.registers.len());
        self.registers[..copy_len].copy_from_slice(&saved[..copy_len]);
    }

    pub(super) fn receiver_slot(
        &self,
        function: &Function,
    ) -> Result<RegisterIndex, InterpreterError> {
        function
            .frame_layout()
            .receiver_slot()
            .ok_or(InterpreterError::RegisterOutOfBounds)
    }

    pub(super) fn receiver(&self, function: &Function) -> Result<RegisterValue, InterpreterError> {
        self.register(self.receiver_slot(function)?)
    }

    pub(super) fn set_receiver(
        &mut self,
        function: &Function,
        value: RegisterValue,
    ) -> Result<(), InterpreterError> {
        self.set_register(self.receiver_slot(function)?, value)
    }

    /// Copies an existing register window into the activation.
    pub fn copy_registers_from_slice(
        &mut self,
        values: &[RegisterValue],
    ) -> Result<(), InterpreterError> {
        if values.len() > self.registers.len() {
            return Err(InterpreterError::RegisterOutOfBounds);
        }

        self.registers[..values.len()].copy_from_slice(values);
        Ok(())
    }

    /// Reads a raw register value.
    pub fn register(&self, index: RegisterIndex) -> Result<RegisterValue, InterpreterError> {
        self.registers
            .get(usize::from(index))
            .copied()
            .ok_or(InterpreterError::RegisterOutOfBounds)
    }

    /// Writes a raw register value.
    pub fn set_register(
        &mut self,
        index: RegisterIndex,
        value: RegisterValue,
    ) -> Result<(), InterpreterError> {
        match self.registers.get_mut(usize::from(index)) {
            Some(slot) => {
                *slot = value;
                self.written_registers.push(index);
                Ok(())
            }
            None => Err(InterpreterError::RegisterOutOfBounds),
        }
    }

    pub(super) fn begin_step(&mut self) {
        self.written_registers.clear();
    }

    pub(super) fn sync_written_open_upvalues(
        &mut self,
        runtime: &mut RuntimeState,
    ) -> Result<(), InterpreterError> {
        let written_registers = std::mem::take(&mut self.written_registers);
        for index in written_registers {
            let Some(upvalue) = self
                .open_upvalues
                .get(usize::from(index))
                .copied()
                .flatten()
            else {
                continue;
            };
            let value = self.register(index)?;
            runtime.objects.set_upvalue(upvalue, value)?;
        }
        Ok(())
    }

    pub(super) fn refresh_open_upvalues_from_cells(
        &mut self,
        runtime: &RuntimeState,
    ) -> Result<(), InterpreterError> {
        for (index, maybe_upvalue) in self.open_upvalues.iter().enumerate() {
            let Some(upvalue) = maybe_upvalue else {
                continue;
            };
            let value = runtime.objects.get_upvalue(*upvalue)?;
            let slot = self
                .registers
                .get_mut(index)
                .ok_or(InterpreterError::RegisterOutOfBounds)?;
            *slot = value;
        }
        Ok(())
    }

    pub(super) fn ensure_open_upvalue(
        &mut self,
        index: RegisterIndex,
        runtime: &mut RuntimeState,
    ) -> Result<ObjectHandle, InterpreterError> {
        if let Some(existing) = self
            .open_upvalues
            .get(usize::from(index))
            .copied()
            .flatten()
        {
            return Ok(existing);
        }

        let value = self.register(index)?;
        let upvalue = runtime.objects.alloc_upvalue(value);
        let slot = self
            .open_upvalues
            .get_mut(usize::from(index))
            .ok_or(InterpreterError::RegisterOutOfBounds)?;
        *slot = Some(upvalue);
        Ok(upvalue)
    }

    pub(super) fn capture_bytecode_register_upvalue(
        &mut self,
        function: &Function,
        runtime: &mut RuntimeState,
        register: BytecodeRegister,
    ) -> Result<ObjectHandle, InterpreterError> {
        let absolute = self.resolve_bytecode_register(function, register.index())?;
        self.ensure_open_upvalue(absolute, runtime)
    }

    pub(super) fn instruction(&self, function: &Function) -> Option<Instruction> {
        function.bytecode().get(self.pc)
    }

    pub(super) fn resolve_bytecode_register(
        &self,
        function: &Function,
        register: RegisterIndex,
    ) -> Result<RegisterIndex, InterpreterError> {
        function
            .frame_layout()
            .resolve_user_visible(register)
            .ok_or(InterpreterError::RegisterOutOfBounds)
    }

    pub(super) fn advance(&mut self) {
        self.pc = self.pc.saturating_add(1);
    }

    pub(super) fn jump_relative(&mut self, offset: i32) -> Result<(), InterpreterError> {
        let current_pc = i64::from(self.pc);
        let target = current_pc + 1 + i64::from(offset);

        if target < 0 {
            return Err(InterpreterError::InvalidJumpTarget);
        }

        self.pc = u32::try_from(target).map_err(|_| InterpreterError::InvalidJumpTarget)?;
        Ok(())
    }

    pub(super) fn read_bytecode_register(
        &self,
        function: &Function,
        register: RegisterIndex,
    ) -> Result<RegisterValue, InterpreterError> {
        let absolute = self.resolve_bytecode_register(function, register)?;
        self.register(absolute)
    }

    pub(super) fn write_bytecode_register(
        &mut self,
        function: &Function,
        register: RegisterIndex,
        value: RegisterValue,
    ) -> Result<(), InterpreterError> {
        let absolute = self.resolve_bytecode_register(function, register)?;
        self.set_register(absolute, value)
    }
}
