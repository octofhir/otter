//! Single call frame: register file, program counter, pending exception,
//! overflow-arguments spill area, open-upvalue bookkeeping, and receiver /
//! new-target plumbing. One `Activation` per active function call.

use crate::bytecode::{BytecodeRegister, ProgramCounter};
use crate::frame::{FrameMetadata, RegisterIndex};
use crate::module::{Function, FunctionIndex};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::InterpreterError;
use super::RuntimeState;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum PendingAbruptCompletion {
    Return(RegisterValue),
    Jump(ProgramCounter),
    Throw(RegisterValue),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct UsingEntry {
    receiver: RegisterValue,
    disposer: ObjectHandle,
    await_dispose: bool,
}

impl UsingEntry {
    #[must_use]
    pub const fn new(receiver: RegisterValue, disposer: ObjectHandle, await_dispose: bool) -> Self {
        Self {
            receiver,
            disposer,
            await_dispose,
        }
    }

    #[must_use]
    pub const fn receiver(self) -> RegisterValue {
        self.receiver
    }

    #[must_use]
    pub const fn disposer(self) -> ObjectHandle {
        self.disposer
    }

    #[must_use]
    pub const fn await_dispose(self) -> bool {
        self.await_dispose
    }
}

/// Mutable activation state for a single executing function frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    function_index: FunctionIndex,
    pub(super) metadata: FrameMetadata,
    closure_handle: Option<ObjectHandle>,
    construct_new_target: Option<ObjectHandle>,
    pending_exception: Option<RegisterValue>,
    pending_abrupt_completion: Option<PendingAbruptCompletion>,
    pending_finally_stack: Vec<ProgramCounter>,
    using_scope_markers: Vec<usize>,
    using_entries: Vec<UsingEntry>,
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
    /// Secondary result slot for ops that produce two values in one
    /// step — notably `IteratorNext` (acc = value, secondary = done
    /// flag) and `PropertyIteratorNext`. Written by the producer op,
    /// consumed by the immediately-following `JumpIfTrue` / similar
    /// branch, so the lifetime is strictly intra-op-pair.
    ///
    /// Defaults to `undefined` on frame creation. Mirrors the
    /// `secondary_result` field already present on `JitContext`.
    pub(super) secondary_result: RegisterValue,
    /// M29: scratch slot for the `AllocClassId` / `CopyClassId`
    /// opcode pair. `AllocClassId` writes a freshly-minted class
    /// identifier here; subsequent `CopyClassId r_target` reads
    /// it and stamps the value onto the closure in `r_target`.
    /// Cleared (set to 0) when the class-definition sequence ends
    /// so stale ids can't leak into sibling class blocks.
    pub(super) current_class_id: u64,
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
            pending_abrupt_completion: None,
            pending_finally_stack: Vec::new(),
            using_scope_markers: Vec::new(),
            using_entries: Vec::new(),
            pc: 0,
            registers: vec![RegisterValue::default(); usize::from(register_count)]
                .into_boxed_slice(),
            open_upvalues: vec![None; usize::from(register_count)].into_boxed_slice(),
            written_registers: Vec::new(),
            overflow_args: Vec::new(),
            accumulator: RegisterValue::undefined(),
            secondary_result: RegisterValue::undefined(),
            current_class_id: 0,
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

    pub(super) fn set_pending_abrupt_completion(&mut self, value: PendingAbruptCompletion) {
        self.pending_abrupt_completion = Some(value);
    }

    #[must_use]
    pub(super) const fn pending_abrupt_completion(&self) -> Option<PendingAbruptCompletion> {
        self.pending_abrupt_completion
    }

    pub(super) fn take_pending_abrupt_completion(&mut self) -> Option<PendingAbruptCompletion> {
        self.pending_abrupt_completion.take()
    }

    pub(super) fn clear_pending_abrupt_completion(&mut self) {
        self.pending_abrupt_completion = None;
    }

    pub(super) fn push_pending_finally(&mut self, target_pc: ProgramCounter) {
        self.pending_finally_stack.push(target_pc);
    }

    pub(super) fn pop_pending_finally(&mut self) -> Option<ProgramCounter> {
        self.pending_finally_stack.pop()
    }

    pub(super) fn clear_pending_finally(&mut self) {
        self.pending_finally_stack.clear();
    }

    pub(super) fn push_using_scope(&mut self) {
        self.using_scope_markers.push(self.using_entries.len());
    }

    pub(super) fn pop_using_scope(&mut self) -> Option<usize> {
        self.using_scope_markers.pop()
    }

    pub(super) fn push_using_entry(&mut self, entry: UsingEntry) {
        self.using_entries.push(entry);
    }

    pub(super) fn pop_using_entry(&mut self) -> Option<UsingEntry> {
        self.using_entries.pop()
    }

    #[must_use]
    pub(super) fn using_entry_count(&self) -> usize {
        self.using_entries.len()
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

    /// Reads the v2 secondary-result slot. Written by ops that produce
    /// two values in one step (e.g. `IteratorNext`) and consumed by the
    /// immediately-following branch op.
    #[must_use]
    pub fn secondary_result(&self) -> RegisterValue {
        self.secondary_result
    }

    /// Overwrites the v2 secondary-result slot.
    pub fn set_secondary_result(&mut self, value: RegisterValue) {
        self.secondary_result = value;
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
