//! Interpreter entry points for the new VM.

use core::fmt;

use crate::bytecode::{BytecodeRegister, Instruction, Opcode, ProgramCounter};
use crate::call::{ClosureCall, DirectCall};
use crate::closure::{ClosureTemplate, UpvalueId};
use crate::descriptors::VmNativeCallError;
use crate::feedback::{FeedbackKind, FeedbackSlotId};
use crate::frame::{FrameFlags, FrameMetadata, RegisterIndex};
use crate::host::{HostFunctionId, NativeFunctionRegistry};
use crate::intrinsics::VmIntrinsics;
use crate::module::{Function, FunctionIndex, Module};
use crate::object::{ObjectError, ObjectHandle, ObjectHeap, PropertyInlineCache, PropertyValue};
use crate::property::{PropertyNameId, PropertyNameRegistry};
use crate::string::StringId;
use crate::value::{RegisterValue, ValueError};

/// Errors produced by the new interpreter.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpreterError {
    /// The bytecode referenced a register outside the current frame layout.
    RegisterOutOfBounds,
    /// The interpreter reached the end of bytecode without an explicit return.
    UnexpectedEndOfBytecode,
    /// A branch jumped outside the valid bytecode range.
    InvalidJumpTarget,
    /// Arithmetic or comparison failed because the inputs were invalid.
    InvalidValue(ValueError),
    /// The current register value is not an object handle.
    InvalidObjectValue,
    /// The current object handle does not exist in the heap.
    InvalidObjectHandle,
    /// The bytecode referenced a missing property-name entry.
    UnknownPropertyName,
    /// The bytecode referenced a missing string-literal entry.
    UnknownStringLiteral,
    /// The bytecode referenced a missing direct-call entry.
    UnknownCallSite,
    /// The direct-call entry referenced a missing callee function.
    InvalidCallTarget,
    /// The bytecode referenced a missing closure-creation entry.
    UnknownClosureTemplate,
    /// The activation attempted to access an upvalue without a closure context.
    MissingClosureContext,
    /// The closure/upvalue slot index is outside the valid range.
    InvalidHeapSlot,
    /// The heap value kind does not support the requested operation.
    InvalidHeapValueKind,
    /// The current handler path expected a pending exception value.
    MissingPendingException,
    /// Execution finished with an uncaught thrown value.
    UncaughtThrow(RegisterValue),
    /// A native host function failed before producing a JS-visible completion.
    NativeCall(Box<str>),
}

impl fmt::Display for InterpreterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegisterOutOfBounds => {
                f.write_str("bytecode referenced a register outside the current frame layout")
            }
            Self::UnexpectedEndOfBytecode => {
                f.write_str("interpreter reached end of bytecode without an explicit return")
            }
            Self::InvalidJumpTarget => {
                f.write_str("branch target is outside the current function bytecode")
            }
            Self::InvalidValue(error) => error.fmt(f),
            Self::InvalidObjectValue => f.write_str("operation expected an object value"),
            Self::InvalidObjectHandle => f.write_str("object handle is outside the current heap"),
            Self::UnknownPropertyName => {
                f.write_str("bytecode referenced a missing property-name entry")
            }
            Self::UnknownStringLiteral => {
                f.write_str("bytecode referenced a missing string-literal entry")
            }
            Self::UnknownCallSite => f.write_str("bytecode referenced a missing direct-call entry"),
            Self::InvalidCallTarget => {
                f.write_str("direct-call entry referenced a missing callee function")
            }
            Self::UnknownClosureTemplate => {
                f.write_str("bytecode referenced a missing closure-creation entry")
            }
            Self::MissingClosureContext => {
                f.write_str("activation attempted to access an upvalue without a closure context")
            }
            Self::InvalidHeapSlot => {
                f.write_str("closure or upvalue slot is outside the valid range")
            }
            Self::InvalidHeapValueKind => {
                f.write_str("operation is not supported for this heap value kind")
            }
            Self::MissingPendingException => {
                f.write_str("handler expected a pending exception value")
            }
            Self::UncaughtThrow(value) => write!(f, "uncaught throw: {:?}", value),
            Self::NativeCall(message) => write!(f, "native host call failed: {message}"),
        }
    }
}

impl std::error::Error for InterpreterError {}

impl From<ValueError> for InterpreterError {
    fn from(value: ValueError) -> Self {
        Self::InvalidValue(value)
    }
}

impl From<ObjectError> for InterpreterError {
    fn from(value: ObjectError) -> Self {
        match value {
            ObjectError::InvalidHandle => Self::InvalidObjectHandle,
            ObjectError::InvalidIndex => Self::InvalidHeapSlot,
            ObjectError::InvalidKind => Self::InvalidHeapValueKind,
        }
    }
}

/// Successful execution result from the interpreter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecutionResult {
    return_value: RegisterValue,
}

impl ExecutionResult {
    /// Creates a successful execution result.
    #[must_use]
    pub const fn new(return_value: RegisterValue) -> Self {
        Self { return_value }
    }

    /// Returns the raw return value.
    #[must_use]
    pub const fn return_value(self) -> RegisterValue {
        self.return_value
    }
}

/// Mutable activation state for a single executing function frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    function_index: FunctionIndex,
    metadata: FrameMetadata,
    closure_handle: Option<ObjectHandle>,
    pending_exception: Option<RegisterValue>,
    pc: ProgramCounter,
    registers: Box<[RegisterValue]>,
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
            pending_exception: None,
            pc: 0,
            registers: vec![RegisterValue::default(); usize::from(register_count)]
                .into_boxed_slice(),
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

    fn set_pending_exception(&mut self, value: RegisterValue) {
        self.pending_exception = Some(value);
    }

    fn take_pending_exception(&mut self) -> Option<RegisterValue> {
        self.pending_exception.take()
    }

    /// Returns the immutable register slice.
    #[must_use]
    pub fn registers(&self) -> &[RegisterValue] {
        &self.registers
    }

    fn receiver_slot(&self, function: &Function) -> Result<RegisterIndex, InterpreterError> {
        function
            .frame_layout()
            .receiver_slot()
            .ok_or(InterpreterError::RegisterOutOfBounds)
    }

    fn receiver(&self, function: &Function) -> Result<RegisterValue, InterpreterError> {
        self.register(self.receiver_slot(function)?)
    }

    fn set_receiver(
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
                Ok(())
            }
            None => Err(InterpreterError::RegisterOutOfBounds),
        }
    }

    fn instruction(&self, function: &Function) -> Option<Instruction> {
        function.bytecode().get(self.pc)
    }

    fn resolve_bytecode_register(
        &self,
        function: &Function,
        register: RegisterIndex,
    ) -> Result<RegisterIndex, InterpreterError> {
        function
            .frame_layout()
            .resolve_user_visible(register)
            .ok_or(InterpreterError::RegisterOutOfBounds)
    }

    fn advance(&mut self) {
        self.pc = self.pc.saturating_add(1);
    }

    fn jump_relative(&mut self, offset: i32) -> Result<(), InterpreterError> {
        let current_pc = i64::from(self.pc);
        let target = current_pc + 1 + i64::from(offset);

        if target < 0 {
            return Err(InterpreterError::InvalidJumpTarget);
        }

        self.pc = u32::try_from(target).map_err(|_| InterpreterError::InvalidJumpTarget)?;
        Ok(())
    }

    fn read_bytecode_register(
        &self,
        function: &Function,
        register: RegisterIndex,
    ) -> Result<RegisterValue, InterpreterError> {
        let absolute = self.resolve_bytecode_register(function, register)?;
        self.register(absolute)
    }

    fn write_bytecode_register(
        &mut self,
        function: &Function,
        register: RegisterIndex,
        value: RegisterValue,
    ) -> Result<(), InterpreterError> {
        let absolute = self.resolve_bytecode_register(function, register)?;
        self.set_register(absolute, value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StepOutcome {
    Continue,
    Return(RegisterValue),
    Throw(RegisterValue),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Completion {
    Return(RegisterValue),
    Throw(RegisterValue),
}

/// Shared execution runtime for one interpreter/JIT run.
pub struct RuntimeState {
    intrinsics: VmIntrinsics,
    objects: ObjectHeap,
    property_names: PropertyNameRegistry,
    native_functions: NativeFunctionRegistry,
}

impl RuntimeState {
    /// Creates a fresh runtime state with an empty object heap.
    #[must_use]
    pub fn new() -> Self {
        let mut objects = ObjectHeap::new();
        let mut intrinsics = VmIntrinsics::allocate(&mut objects);
        let mut property_names = PropertyNameRegistry::new();
        let mut native_functions = NativeFunctionRegistry::new();
        intrinsics
            .wire_prototype_chains(&mut objects)
            .expect("intrinsic prototype wiring should bootstrap cleanly");
        intrinsics
            .init_core(&mut objects, &mut property_names, &mut native_functions)
            .expect("intrinsic core init should bootstrap cleanly");
        intrinsics
            .install_on_global(&mut objects, &mut property_names, &mut native_functions)
            .expect("intrinsic global install should bootstrap cleanly");

        Self {
            intrinsics,
            objects,
            property_names,
            native_functions,
        }
    }

    /// Returns the intrinsic registry owned by the runtime.
    #[must_use]
    pub fn intrinsics(&self) -> &VmIntrinsics {
        &self.intrinsics
    }

    /// Returns the mutable intrinsic registry owned by the runtime.
    pub fn intrinsics_mut(&mut self) -> &mut VmIntrinsics {
        &mut self.intrinsics
    }

    /// Returns the current object heap.
    #[must_use]
    pub fn objects(&self) -> &ObjectHeap {
        &self.objects
    }

    /// Returns the mutable object heap.
    pub fn objects_mut(&mut self) -> &mut ObjectHeap {
        &mut self.objects
    }

    /// Returns the runtime-wide property-name registry.
    #[must_use]
    pub fn property_names(&self) -> &PropertyNameRegistry {
        &self.property_names
    }

    /// Returns the mutable runtime-wide property-name registry.
    pub fn property_names_mut(&mut self) -> &mut PropertyNameRegistry {
        &mut self.property_names
    }

    /// Interns one property name into the runtime-wide registry.
    pub fn intern_property_name(&mut self, name: &str) -> PropertyNameId {
        self.property_names.intern(name)
    }

    /// Returns the runtime-wide native host-function registry.
    #[must_use]
    pub fn native_functions(&self) -> &NativeFunctionRegistry {
        &self.native_functions
    }

    /// Returns the mutable runtime-wide native host-function registry.
    pub fn native_functions_mut(&mut self) -> &mut NativeFunctionRegistry {
        &mut self.native_functions
    }

    /// Registers one host-callable native function in the runtime registry.
    pub fn register_native_function(
        &mut self,
        descriptor: crate::descriptors::NativeFunctionDescriptor,
    ) -> HostFunctionId {
        self.native_functions.register(descriptor)
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct FrameRuntimeState {
    property_feedback: Box<[Option<PropertyInlineCache>]>,
}

impl FrameRuntimeState {
    fn new(function: &Function) -> Self {
        Self {
            property_feedback: vec![None; function.feedback().len()].into_boxed_slice(),
        }
    }

    fn property_cache(
        &self,
        function: &Function,
        pc: ProgramCounter,
    ) -> Option<PropertyInlineCache> {
        let index = Self::property_feedback_index(function, pc)?;
        self.property_feedback[index]
    }

    fn update_property_cache(
        &mut self,
        function: &Function,
        pc: ProgramCounter,
        cache: PropertyInlineCache,
    ) {
        let Some(index) = Self::property_feedback_index(function, pc) else {
            return;
        };
        self.property_feedback[index] = Some(cache);
    }

    fn property_feedback_index(function: &Function, pc: ProgramCounter) -> Option<usize> {
        let slot = FeedbackSlotId(u16::try_from(pc).ok()?);
        let layout = function.feedback().get(slot)?;
        (layout.kind() == FeedbackKind::Property).then_some(usize::from(slot.0))
    }
}

/// Minimal interpreter shell for the new VM backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Interpreter;

impl Interpreter {
    /// Creates a new interpreter instance.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Creates an entry activation for the module entry function.
    #[must_use]
    pub fn prepare_entry(module: &Module) -> Activation {
        let function = module.entry_function();
        let register_count = function.frame_layout().register_count();
        let mut activation = Activation::new(module.entry(), register_count);
        if function.frame_layout().receiver_slot().is_some() {
            activation
                .set_receiver(function, RegisterValue::undefined())
                .expect("entry receiver slot must exist when reserved");
        }
        activation
    }

    /// Executes a module from its entry function.
    pub fn execute(&self, module: &Module) -> Result<ExecutionResult, InterpreterError> {
        let mut activation = Self::prepare_entry(module);
        let mut runtime = RuntimeState::new();
        self.run_with_runtime(module, &mut activation, &mut runtime)
    }

    /// Runs an existing activation until it returns or traps.
    pub fn run(
        &self,
        module: &Module,
        activation: &mut Activation,
    ) -> Result<ExecutionResult, InterpreterError> {
        let mut runtime = RuntimeState::new();
        self.run_with_runtime(module, activation, &mut runtime)
    }

    /// Executes one function on an existing runtime from a prepared register window.
    pub fn execute_with_runtime(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        registers: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        self.resume_with_runtime(module, function_index, 0, registers, runtime)
    }

    /// Resumes one function from an explicit PC and pre-materialized register window.
    pub fn resume(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        resume_pc: ProgramCounter,
        registers: &[RegisterValue],
    ) -> Result<ExecutionResult, InterpreterError> {
        let function = module
            .function(function_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation =
            Activation::new(function_index, function.frame_layout().register_count());
        activation.copy_registers_from_slice(registers)?;
        activation.set_pc(resume_pc);

        let mut runtime = RuntimeState::new();
        self.run_with_runtime(module, &mut activation, &mut runtime)
    }

    /// Resumes one function on an existing runtime from an explicit PC.
    pub fn resume_with_runtime(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        resume_pc: ProgramCounter,
        registers: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        let function = module
            .function(function_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation =
            Activation::new(function_index, function.frame_layout().register_count());
        activation.copy_registers_from_slice(registers)?;
        activation.set_pc(resume_pc);

        self.run_with_runtime(module, &mut activation, runtime)
    }

    /// Profiles monomorphic property caches for one function on a fresh runtime.
    pub fn profile_property_caches(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        registers: &[RegisterValue],
    ) -> Result<Box<[Option<PropertyInlineCache>]>, InterpreterError> {
        let function = module
            .function(function_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation =
            Activation::new(function_index, function.frame_layout().register_count());
        activation.copy_registers_from_slice(registers)?;
        let mut runtime = RuntimeState::new();
        let mut frame_runtime = FrameRuntimeState::new(function);

        loop {
            match self.step(
                function,
                module,
                &mut activation,
                &mut runtime,
                &mut frame_runtime,
            )? {
                StepOutcome::Continue => {}
                StepOutcome::Return(_) => {
                    return Ok(frame_runtime.property_feedback);
                }
                StepOutcome::Throw(value) => {
                    return Err(InterpreterError::UncaughtThrow(value));
                }
            }
        }
    }

    fn run_with_runtime(
        &self,
        module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        match self.run_completion_with_runtime(module, activation, runtime)? {
            Completion::Return(return_value) => Ok(ExecutionResult::new(return_value)),
            Completion::Throw(value) => Err(InterpreterError::UncaughtThrow(value)),
        }
    }

    fn run_completion_with_runtime(
        &self,
        module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
    ) -> Result<Completion, InterpreterError> {
        let function = module
            .function(activation.function_index())
            .expect("activation function index must be valid");
        let mut frame_runtime = FrameRuntimeState::new(function);

        loop {
            match self.step(function, module, activation, runtime, &mut frame_runtime)? {
                StepOutcome::Continue => {}
                StepOutcome::Return(return_value) => {
                    return Ok(Completion::Return(return_value));
                }
                StepOutcome::Throw(value) => {
                    if self.transfer_exception(function, activation, value) {
                        continue;
                    }
                    return Ok(Completion::Throw(value));
                }
            }
        }
    }

    fn transfer_exception(
        &self,
        function: &Function,
        activation: &mut Activation,
        value: RegisterValue,
    ) -> bool {
        let Some(handler) = function.exceptions().find_handler(activation.pc()) else {
            return false;
        };

        activation.set_pending_exception(value);
        activation.set_pc(handler.handler_pc());
        true
    }

    fn step(
        &self,
        function: &Function,
        module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
        frame_runtime: &mut FrameRuntimeState,
    ) -> Result<StepOutcome, InterpreterError> {
        let instruction = activation
            .instruction(function)
            .ok_or(InterpreterError::UnexpectedEndOfBytecode)?;

        match instruction.opcode() {
            Opcode::Nop => {
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Move => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadI32 => {
                let value = RegisterValue::from_i32(instruction.immediate_i32());
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadTrue => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(true),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadFalse => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(false),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewObject => {
                let handle = runtime.objects.alloc_object();
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadString => {
                let string = Self::resolve_string_literal(function, instruction.b())?;
                let handle = runtime.objects.alloc_string(string);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewArray => {
                let handle = runtime.objects.alloc_array();
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewClosure => {
                let template = Self::resolve_closure_template(function, activation.pc())?;
                let capture_start = instruction.b();
                let mut upvalues = Vec::with_capacity(usize::from(template.capture_count()));

                for offset in 0..template.capture_count() {
                    let value = activation
                        .read_bytecode_register(function, capture_start.saturating_add(offset))?;
                    upvalues.push(runtime.objects.alloc_upvalue(value));
                }

                let handle = runtime.objects.alloc_closure(template.callee(), upvalues);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadUndefined => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::undefined(),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadNull => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::null(),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadException => {
                let value = activation
                    .take_pending_exception()
                    .ok_or(InterpreterError::MissingPendingException)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadCurrentClosure => {
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(closure.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadThis => {
                let receiver = activation.receiver(function)?;
                activation.write_bytecode_register(function, instruction.a(), receiver)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Not => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(!value.is_truthy()),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Add => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let value = lhs.add_i32(rhs)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Sub => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let value = lhs.sub_i32(rhs)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Mul => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let value = lhs.mul_i32(rhs)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Div => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let value = lhs.div_i32(rhs)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Eq => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(runtime.objects.strict_eq(lhs, rhs)?),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Lt => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let value = lhs.lt(rhs)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetProperty => {
                let pc = activation.pc();
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let handle = Self::read_object_handle(activation, function, instruction.b())?;
                let property_name = runtime
                    .property_names()
                    .get(property)
                    .expect("resolved runtime property name must exist");

                if let Some(value) = runtime
                    .objects
                    .get_builtin_property(handle, property_name)?
                {
                    activation.write_bytecode_register(function, instruction.a(), value)?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let value = if let Some(cache) = frame_runtime.property_cache(function, pc) {
                    match runtime.objects.get_cached(handle, property, cache)? {
                        Some(PropertyValue::Data(value)) => value,
                        Some(PropertyValue::Accessor { getter, .. }) => {
                            Self::invoke_accessor_getter(runtime, handle, getter)?
                        }
                        None => Self::generic_get_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            property,
                        )?,
                    }
                } else {
                    Self::generic_get_property(
                        function,
                        runtime,
                        frame_runtime,
                        pc,
                        handle,
                        property,
                    )?
                };

                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetProperty => {
                let pc = activation.pc();
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let handle = Self::read_object_handle(activation, function, instruction.a())?;
                let value = activation.read_bytecode_register(function, instruction.b())?;

                let handled = if let Some(cache) = frame_runtime.property_cache(function, pc) {
                    match runtime.objects.get_cached(handle, property, cache)? {
                        Some(PropertyValue::Data(_)) => {
                            runtime.objects.set_cached(handle, property, value, cache)?
                        }
                        Some(PropertyValue::Accessor { setter, .. }) => {
                            Self::invoke_accessor_setter(runtime, handle, setter, value)?;
                            true
                        }
                        None => Self::generic_set_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            property,
                            value,
                        )?,
                    }
                } else {
                    Self::generic_set_property(
                        function,
                        runtime,
                        frame_runtime,
                        pc,
                        handle,
                        property,
                        value,
                    )?
                };

                if !handled {
                    let cache = runtime.objects.set_property(handle, property, value)?;
                    frame_runtime.update_property_cache(function, pc, cache);
                }

                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetIndex => {
                let handle = Self::read_object_handle(activation, function, instruction.b())?;
                let index = Self::read_index(activation, function, instruction.c())?;
                let value = runtime
                    .objects
                    .get_index(handle, index)?
                    .unwrap_or_else(RegisterValue::undefined);
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetIndex => {
                let handle = Self::read_object_handle(activation, function, instruction.a())?;
                let index = Self::read_index(activation, function, instruction.b())?;
                let value = activation.read_bytecode_register(function, instruction.c())?;
                runtime.objects.set_index(handle, index, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetIterator => {
                let handle = Self::read_object_handle(activation, function, instruction.b())?;
                let iterator = runtime.objects.alloc_iterator(handle)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(iterator.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::IteratorNext => {
                let iterator = Self::read_object_handle(activation, function, instruction.c())?;
                let step = runtime.objects.iterator_next(iterator)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(step.is_done()),
                )?;
                activation.write_bytecode_register(function, instruction.b(), step.value())?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::IteratorClose => {
                let iterator = Self::read_object_handle(activation, function, instruction.a())?;
                runtime.objects.iterator_close(iterator)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetUpvalue => {
                let upvalue =
                    Self::resolve_upvalue_cell(activation, runtime, UpvalueId(instruction.b()))?;
                let value = runtime.objects.get_upvalue(upvalue)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetUpvalue => {
                let upvalue =
                    Self::resolve_upvalue_cell(activation, runtime, UpvalueId(instruction.b()))?;
                let value = activation.read_bytecode_register(function, instruction.a())?;
                runtime.objects.set_upvalue(upvalue, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CallDirect => {
                let call = Self::resolve_direct_call(function, activation.pc())?;
                let mut callee_activation =
                    Self::prepare_direct_call(module, function, activation, instruction.b(), call)?;
                match self.run_completion_with_runtime(module, &mut callee_activation, runtime)? {
                    Completion::Return(value) => {
                        activation.write_bytecode_register(function, instruction.a(), value)?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                }
            }
            Opcode::CallClosure => {
                let call = Self::resolve_closure_call(function, activation.pc())?;
                let caller_function = module
                    .function(activation.function_index())
                    .expect("activation function index must be valid");
                let callee = activation
                    .read_bytecode_register(caller_function, instruction.b())?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;

                if let Some(host_function) = runtime.objects.host_function(callee)? {
                    match Self::invoke_host_function(
                        caller_function,
                        activation,
                        runtime,
                        host_function,
                        instruction.c(),
                        call,
                    )? {
                        Completion::Return(value) => {
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            Ok(StepOutcome::Continue)
                        }
                        Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                    }
                } else {
                    let mut callee_activation = Self::prepare_closure_call(
                        module,
                        activation,
                        runtime,
                        instruction.b(),
                        instruction.c(),
                        call,
                    )?;
                    match self.run_completion_with_runtime(
                        module,
                        &mut callee_activation,
                        runtime,
                    )? {
                        Completion::Return(value) => {
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            Ok(StepOutcome::Continue)
                        }
                        Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                    }
                }
            }
            Opcode::Jump => {
                activation.jump_relative(instruction.immediate_i32())?;
                Ok(StepOutcome::Continue)
            }
            Opcode::JumpIfTrue => {
                let condition = activation.read_bytecode_register(function, instruction.a())?;
                if condition.is_truthy() {
                    activation.jump_relative(instruction.immediate_i32())?;
                } else {
                    activation.advance();
                }
                Ok(StepOutcome::Continue)
            }
            Opcode::JumpIfFalse => {
                let condition = activation.read_bytecode_register(function, instruction.a())?;
                if condition.is_truthy() {
                    activation.advance();
                } else {
                    activation.jump_relative(instruction.immediate_i32())?;
                }
                Ok(StepOutcome::Continue)
            }
            Opcode::Return => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                Ok(StepOutcome::Return(value))
            }
            Opcode::Throw => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                Ok(StepOutcome::Throw(value))
            }
        }
    }

    fn resolve_property_name(
        function: &Function,
        runtime: &mut RuntimeState,
        raw_id: RegisterIndex,
    ) -> Result<PropertyNameId, InterpreterError> {
        let property_name = function
            .property_names()
            .get(PropertyNameId(raw_id))
            .ok_or(InterpreterError::UnknownPropertyName)?;
        Ok(runtime.intern_property_name(property_name))
    }

    fn resolve_string_literal(
        function: &Function,
        raw_id: RegisterIndex,
    ) -> Result<&str, InterpreterError> {
        function
            .string_literals()
            .get(StringId(raw_id))
            .ok_or(InterpreterError::UnknownStringLiteral)
    }

    fn resolve_closure_template(
        function: &Function,
        pc: ProgramCounter,
    ) -> Result<ClosureTemplate, InterpreterError> {
        function
            .closures()
            .get(pc)
            .ok_or(InterpreterError::UnknownClosureTemplate)
    }

    fn resolve_direct_call(
        function: &Function,
        pc: ProgramCounter,
    ) -> Result<DirectCall, InterpreterError> {
        function
            .calls()
            .get_direct(pc)
            .ok_or(InterpreterError::UnknownCallSite)
    }

    fn resolve_closure_call(
        function: &Function,
        pc: ProgramCounter,
    ) -> Result<ClosureCall, InterpreterError> {
        function
            .calls()
            .get_closure(pc)
            .ok_or(InterpreterError::UnknownCallSite)
    }

    fn read_object_handle(
        activation: &Activation,
        function: &Function,
        register: RegisterIndex,
    ) -> Result<ObjectHandle, InterpreterError> {
        let value = activation.read_bytecode_register(function, register)?;
        value
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(InterpreterError::InvalidObjectValue)
    }

    fn generic_get_property(
        function: &Function,
        runtime: &mut RuntimeState,
        frame_runtime: &mut FrameRuntimeState,
        pc: ProgramCounter,
        handle: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<RegisterValue, InterpreterError> {
        match runtime.objects.get_property(handle, property)? {
            Some((PropertyValue::Data(value), cache)) => {
                frame_runtime.update_property_cache(function, pc, cache);
                Ok(value)
            }
            Some((PropertyValue::Accessor { getter, .. }, cache)) => {
                frame_runtime.update_property_cache(function, pc, cache);
                Self::invoke_accessor_getter(runtime, handle, getter)
            }
            None => Ok(RegisterValue::undefined()),
        }
    }

    fn generic_set_property(
        function: &Function,
        runtime: &mut RuntimeState,
        frame_runtime: &mut FrameRuntimeState,
        pc: ProgramCounter,
        handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        match runtime.objects.get_property(handle, property)? {
            Some((PropertyValue::Data(_), cache)) => {
                frame_runtime.update_property_cache(function, pc, cache);
                Ok(runtime.objects.set_cached(handle, property, value, cache)?)
            }
            Some((PropertyValue::Accessor { setter, .. }, cache)) => {
                frame_runtime.update_property_cache(function, pc, cache);
                Self::invoke_accessor_setter(runtime, handle, setter, value)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn read_index(
        activation: &Activation,
        function: &Function,
        register: RegisterIndex,
    ) -> Result<usize, InterpreterError> {
        let value = activation.read_bytecode_register(function, register)?;
        let index = value.as_i32().ok_or(ValueError::ExpectedI32)?;
        usize::try_from(index).map_err(|_| InterpreterError::InvalidValue(ValueError::ExpectedI32))
    }

    fn resolve_upvalue_cell(
        activation: &Activation,
        runtime: &RuntimeState,
        upvalue: UpvalueId,
    ) -> Result<ObjectHandle, InterpreterError> {
        let closure = activation
            .closure_handle()
            .ok_or(InterpreterError::MissingClosureContext)?;
        runtime
            .objects
            .closure_upvalue(closure, usize::from(upvalue.0))
            .map_err(Into::into)
    }

    fn prepare_direct_call(
        module: &Module,
        caller_function: &Function,
        caller_activation: &Activation,
        arg_start: RegisterIndex,
        call: DirectCall,
    ) -> Result<Activation, InterpreterError> {
        let callee = module
            .function(call.callee())
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation = Activation::with_context(
            call.callee(),
            callee.frame_layout().register_count(),
            FrameMetadata::new(call.argument_count(), call.flags()),
            None,
        );
        let parameter_range = callee.frame_layout().parameter_range();
        let copy_count = call.argument_count().min(parameter_range.len());

        for offset in 0..copy_count {
            let value = caller_activation
                .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
            activation.set_register(parameter_range.start().saturating_add(offset), value)?;
        }

        Self::initialize_receiver(
            caller_function,
            caller_activation,
            callee,
            &mut activation,
            call.flags(),
            call.receiver(),
        )?;

        Ok(activation)
    }

    fn prepare_closure_call(
        module: &Module,
        caller_activation: &Activation,
        runtime: &RuntimeState,
        callee_register: RegisterIndex,
        arg_start: RegisterIndex,
        call: ClosureCall,
    ) -> Result<Activation, InterpreterError> {
        let closure = caller_activation
            .read_bytecode_register(
                module
                    .function(caller_activation.function_index())
                    .expect("activation function index must be valid"),
                callee_register,
            )?
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(InterpreterError::InvalidObjectValue)?;
        let callee_index = runtime.objects.closure_callee(closure)?;
        let callee = module
            .function(callee_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let mut activation = Activation::with_context(
            callee_index,
            callee.frame_layout().register_count(),
            FrameMetadata::new(call.argument_count(), call.flags()),
            Some(closure),
        );
        let caller_function = module
            .function(caller_activation.function_index())
            .expect("activation function index must be valid");
        let parameter_range = callee.frame_layout().parameter_range();
        let copy_count = call.argument_count().min(parameter_range.len());

        for offset in 0..copy_count {
            let value = caller_activation
                .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
            activation.set_register(parameter_range.start().saturating_add(offset), value)?;
        }

        Self::initialize_receiver(
            caller_function,
            caller_activation,
            callee,
            &mut activation,
            call.flags(),
            call.receiver(),
        )?;

        Ok(activation)
    }

    fn invoke_host_function(
        caller_function: &Function,
        caller_activation: &Activation,
        runtime: &mut RuntimeState,
        host_function: HostFunctionId,
        arg_start: RegisterIndex,
        call: ClosureCall,
    ) -> Result<Completion, InterpreterError> {
        let receiver = Self::resolve_call_receiver(
            caller_function,
            caller_activation,
            call.flags(),
            call.receiver(),
        )?;
        let arguments = Self::read_call_arguments(
            caller_function,
            caller_activation,
            arg_start,
            call.argument_count(),
        )?;
        Self::invoke_registered_host_function(runtime, host_function, receiver, &arguments)
    }

    fn read_call_arguments(
        caller_function: &Function,
        caller_activation: &Activation,
        arg_start: RegisterIndex,
        argument_count: RegisterIndex,
    ) -> Result<Vec<RegisterValue>, InterpreterError> {
        let mut arguments = Vec::with_capacity(usize::from(argument_count));
        for offset in 0..argument_count {
            let value = caller_activation
                .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
            arguments.push(value);
        }
        Ok(arguments)
    }

    fn resolve_call_receiver(
        caller_function: &Function,
        caller_activation: &Activation,
        flags: FrameFlags,
        receiver_register: Option<BytecodeRegister>,
    ) -> Result<RegisterValue, InterpreterError> {
        match receiver_register {
            Some(receiver_register) => {
                caller_activation.read_bytecode_register(caller_function, receiver_register.index())
            }
            None if flags.has_receiver() => Ok(RegisterValue::undefined()),
            None => Ok(RegisterValue::undefined()),
        }
    }

    fn invoke_accessor_getter(
        runtime: &mut RuntimeState,
        receiver_handle: ObjectHandle,
        getter: Option<ObjectHandle>,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(getter) = getter else {
            return Ok(RegisterValue::undefined());
        };

        match Self::invoke_host_function_handle(
            runtime,
            getter,
            RegisterValue::from_object_handle(receiver_handle.0),
            &[],
        )? {
            Completion::Return(value) => Ok(value),
            Completion::Throw(value) => Err(InterpreterError::UncaughtThrow(value)),
        }
    }

    fn invoke_accessor_setter(
        runtime: &mut RuntimeState,
        receiver_handle: ObjectHandle,
        setter: Option<ObjectHandle>,
        value: RegisterValue,
    ) -> Result<(), InterpreterError> {
        let Some(setter) = setter else {
            return Ok(());
        };

        match Self::invoke_host_function_handle(
            runtime,
            setter,
            RegisterValue::from_object_handle(receiver_handle.0),
            &[value],
        )? {
            Completion::Return(_) => Ok(()),
            Completion::Throw(value) => Err(InterpreterError::UncaughtThrow(value)),
        }
    }

    fn invoke_host_function_handle(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<Completion, InterpreterError> {
        let host_function = runtime
            .objects
            .host_function(callable)?
            .ok_or(InterpreterError::InvalidCallTarget)?;
        Self::invoke_registered_host_function(runtime, host_function, receiver, arguments)
    }

    fn invoke_registered_host_function(
        runtime: &mut RuntimeState,
        host_function: HostFunctionId,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<Completion, InterpreterError> {
        let descriptor = runtime
            .native_functions()
            .get(host_function)
            .cloned()
            .ok_or(InterpreterError::InvalidCallTarget)?;

        match (descriptor.callback())(&receiver, arguments, runtime) {
            Ok(value) => Ok(Completion::Return(value)),
            Err(VmNativeCallError::Thrown(value)) => Ok(Completion::Throw(value)),
            Err(VmNativeCallError::Internal(message)) => Err(InterpreterError::NativeCall(message)),
        }
    }

    fn initialize_receiver(
        caller_function: &Function,
        caller_activation: &Activation,
        callee_function: &Function,
        callee_activation: &mut Activation,
        flags: FrameFlags,
        receiver_register: Option<BytecodeRegister>,
    ) -> Result<(), InterpreterError> {
        let receiver = match receiver_register {
            Some(receiver_register) => caller_activation
                .read_bytecode_register(caller_function, receiver_register.index())?,
            None if flags.has_receiver()
                || callee_function.frame_layout().receiver_slot().is_some() =>
            {
                RegisterValue::undefined()
            }
            None => return Ok(()),
        };

        if callee_function.frame_layout().receiver_slot().is_some() {
            callee_activation.set_receiver(callee_function, receiver)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset};
    use crate::call::{CallSite, CallTable, ClosureCall, DirectCall};
    use crate::closure::{ClosureTable, ClosureTemplate, UpvalueId};
    use crate::deopt::DeoptTable;
    use crate::exception::ExceptionTable;
    use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
    use crate::frame::{FrameFlags, FrameLayout};
    use crate::module::{Function, FunctionIndex, FunctionSideTables, FunctionTables, Module};
    use crate::property::PropertyNameTable;
    use crate::source_map::SourceMap;
    use crate::string::StringTable;
    use crate::value::{RegisterValue, ValueError};

    use super::{Activation, ExecutionResult, Interpreter, InterpreterError, RuntimeState};

    #[test]
    fn interpreter_executes_nop_then_return() {
        let layout = FrameLayout::new(0, 1, 0, 0).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::nop(),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");
        let interpreter = Interpreter::new();
        let mut activation = Interpreter::prepare_entry(&module);
        activation
            .set_register(layout.user_visible_start(), RegisterValue::from_i32(7))
            .expect("register should exist");

        let result = interpreter.run(&module, &mut activation);

        assert_eq!(result, Ok(ExecutionResult::new(RegisterValue::from_i32(7))));
        assert_eq!(activation.pc(), 1);
    }

    #[test]
    fn interpreter_executes_arithmetic_program() {
        let layout = FrameLayout::new(1, 0, 0, 7).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 20),
                Instruction::load_i32(BytecodeRegister::new(1), 22),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::sub(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                ),
                Instruction::mul(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                ),
                Instruction::load_i32(BytecodeRegister::new(5), 2),
                Instruction::div(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(5),
                ),
                Instruction::ret(BytecodeRegister::new(6)),
            ]),
        );
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(242))
        );
    }

    #[test]
    fn interpreter_reports_unexpected_end_of_bytecode() {
        let function =
            Function::with_bytecode(Some("entry"), FrameLayout::default(), Bytecode::default());
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(result, Err(InterpreterError::UnexpectedEndOfBytecode));
    }

    #[test]
    fn interpreter_executes_loop_with_conditional_branch() {
        let layout = FrameLayout::new(0, 0, 0, 5).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 0),
                Instruction::load_i32(BytecodeRegister::new(1), 4),
                Instruction::load_i32(BytecodeRegister::new(2), 0),
                Instruction::load_i32(BytecodeRegister::new(3), 1),
                Instruction::lt(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(4), JumpOffset::new(3)),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                ),
                Instruction::add(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(3),
                ),
                Instruction::jump(JumpOffset::new(-5)),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
        );
        let module = Module::new(Some("loop"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(6))
        );
    }

    #[test]
    fn interpreter_rejects_invalid_jump_target() {
        let layout = FrameLayout::new(0, 0, 0, 1).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![Instruction::jump(JumpOffset::new(-2))]),
        );
        let module = Module::new(Some("invalid-jump"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(result, Err(InterpreterError::InvalidJumpTarget));
    }

    #[test]
    fn interpreter_rejects_invalid_arithmetic_operands() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_true(BytecodeRegister::new(0)),
                Instruction::load_i32(BytecodeRegister::new(1), 1),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
            ]),
        );
        let module = Module::new(Some("invalid-add"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result,
            Err(InterpreterError::InvalidValue(ValueError::ExpectedI32))
        );
    }

    #[test]
    fn interpreter_executes_object_property_round_trip() {
        let layout = FrameLayout::new(0, 0, 0, 4).expect("frame layout should be valid");
        let bytecode = Bytecode::from(vec![
            Instruction::new_object(BytecodeRegister::new(0)),
            Instruction::load_i32(BytecodeRegister::new(1), 7),
            Instruction::set_property(
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
                crate::property::PropertyNameId(0),
            ),
            Instruction::get_property(
                BytecodeRegister::new(2),
                BytecodeRegister::new(0),
                crate::property::PropertyNameId(0),
            ),
            Instruction::ret(BytecodeRegister::new(2)),
        ]);
        let feedback = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
            FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
            FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
            FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
        ]);
        let function = Function::new(
            Some("entry"),
            layout,
            bytecode,
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["count"]),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                feedback,
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("object"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_rejects_invalid_object_value() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let function = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 1),
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["count"]),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("object"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(result, Err(InterpreterError::InvalidObjectValue));
    }

    #[test]
    fn interpreter_executes_string_and_array_fast_paths() {
        let layout = FrameLayout::new(0, 0, 0, 10).expect("frame layout should be valid");
        let bytecode = Bytecode::from(vec![
            Instruction::load_string(BytecodeRegister::new(0), crate::string::StringId(0)),
            Instruction::get_property(
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
                crate::property::PropertyNameId(0),
            ),
            Instruction::new_array(BytecodeRegister::new(2)),
            Instruction::load_i32(BytecodeRegister::new(3), 0),
            Instruction::set_index(
                BytecodeRegister::new(2),
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
            ),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::get_index(
                BytecodeRegister::new(5),
                BytecodeRegister::new(0),
                BytecodeRegister::new(4),
            ),
            Instruction::set_index(
                BytecodeRegister::new(2),
                BytecodeRegister::new(4),
                BytecodeRegister::new(5),
            ),
            Instruction::get_index(
                BytecodeRegister::new(6),
                BytecodeRegister::new(2),
                BytecodeRegister::new(3),
            ),
            Instruction::get_property(
                BytecodeRegister::new(7),
                BytecodeRegister::new(2),
                crate::property::PropertyNameId(0),
            ),
            Instruction::add(
                BytecodeRegister::new(8),
                BytecodeRegister::new(6),
                BytecodeRegister::new(7),
            ),
            Instruction::ret(BytecodeRegister::new(8)),
        ]);
        let function = Function::new(
            Some("entry"),
            layout,
            bytecode,
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["length"]),
                    StringTable::new(vec!["otter"]),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(8), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(9), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(10), FeedbackKind::Arithmetic),
                    FeedbackSlotLayout::new(FeedbackSlotId(11), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("string-array"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_executes_direct_call_with_contiguous_argument_window() {
        let entry_layout = FrameLayout::new(0, 0, 0, 4).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(0, 2, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 20),
                Instruction::load_i32(BytecodeRegister::new(1), 22),
                Instruction::call_direct(BytecodeRegister::new(2), BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        Some(CallSite::Direct(DirectCall::new(
                            FunctionIndex(1),
                            2,
                            FrameFlags::empty(),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let helper = Function::with_bytecode(
            Some("helper"),
            helper_layout,
            Bytecode::from(vec![
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
        );
        let module = Module::new(Some("direct-call"), vec![entry, helper], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(42))
        );
    }

    #[test]
    fn interpreter_shares_property_names_across_function_tables() {
        let entry_layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(0, 1, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::new_object(BytecodeRegister::new(0)),
                Instruction::load_i32(BytecodeRegister::new(1), 7),
                Instruction::set_property(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::call_direct(BytecodeRegister::new(2), BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["ignored", "shared"]),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        Some(CallSite::Direct(DirectCall::new(
                            FunctionIndex(1),
                            1,
                            FrameFlags::empty(),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let helper = Function::new(
            Some("helper"),
            helper_layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::ret(BytecodeRegister::new(1)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["shared"]),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(
            Some("cross-function-property"),
            vec![entry, helper],
            FunctionIndex(0),
        )
        .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_calls_bootstrap_installed_math_abs() {
        let layout = FrameLayout::new(0, 0, 0, 5).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::load_i32(BytecodeRegister::new(3), -7),
                Instruction::call_closure(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(3),
                ),
                Instruction::ret(BytecodeRegister::new(4)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["Math", "abs"]),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            1,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(1),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("math-abs"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_reads_and_writes_bootstrap_installed_math_accessor() {
        let layout = FrameLayout::new(0, 0, 0, 4).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::load_i32(BytecodeRegister::new(2), 7),
                Instruction::set_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(2),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::ret(BytecodeRegister::new(3)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["Math", "memory"]),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("math-accessor"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_calls_bootstrap_installed_object_static_and_prototype_methods() {
        let layout = FrameLayout::new(0, 0, 0, 8).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(2),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(4),
                    crate::property::PropertyNameId(3),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(3),
                ),
                Instruction::eq(
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(3),
                ),
                Instruction::ret(BytecodeRegister::new(7)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["Object", "create", "prototype", "valueOf"]),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(1),
                        ))),
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(3),
                        ))),
                        None,
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Comparison),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("object-bootstrap"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_bool(true))
        );
    }

    #[test]
    fn interpreter_calls_bootstrap_installed_function_static_and_prototype_methods() {
        let layout = FrameLayout::new(0, 0, 0, 11).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(2),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(3),
                    crate::property::PropertyNameId(3),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(2),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(5), JumpOffset::new(6)),
                Instruction::get_property(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(3),
                    crate::property::PropertyNameId(4),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(6),
                    crate::property::PropertyNameId(5),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(8),
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(0),
                ),
                Instruction::load_string(BytecodeRegister::new(9), crate::string::StringId(0)),
                Instruction::eq(
                    BytecodeRegister::new(10),
                    BytecodeRegister::new(8),
                    BytecodeRegister::new(9),
                ),
                Instruction::ret(BytecodeRegister::new(10)),
                Instruction::load_false(BytecodeRegister::new(10)),
                Instruction::ret(BytecodeRegister::new(10)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec![
                        "Math",
                        "abs",
                        "Function",
                        "isCallable",
                        "prototype",
                        "toString",
                    ]),
                    StringTable::new(vec!["function () { [native code] }"]),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            1,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(3),
                        ))),
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(2),
                        ))),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(8), FeedbackKind::Comparison),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("function-bootstrap"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_bool(true))
        );
    }

    #[test]
    fn interpreter_ordinary_calls_default_this_to_undefined() {
        let entry_layout = FrameLayout::new(0, 0, 0, 2).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(1, 0, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::call_direct(BytecodeRegister::new(0), BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Direct(DirectCall::new(
                            FunctionIndex(1),
                            0,
                            FrameFlags::empty(),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let helper = Function::with_bytecode(
            Some("helper"),
            helper_layout,
            Bytecode::from(vec![
                Instruction::load_this(BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );
        let module = Module::new(Some("ordinary-this"), vec![entry, helper], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::undefined())
        );
    }

    #[test]
    fn interpreter_method_calls_preserve_receiver_in_hidden_slot() {
        let entry_layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let closure_layout = FrameLayout::new(1, 0, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::new_object(BytecodeRegister::new(0)),
                Instruction::new_closure(BytecodeRegister::new(1), BytecodeRegister::new(0)),
                Instruction::call_closure(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    ClosureTable::new(vec![
                        None,
                        Some(ClosureTemplate::new(FunctionIndex(1), 0)),
                        None,
                        None,
                    ]),
                    CallTable::new(vec![
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(0),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let closure = Function::with_bytecode(
            Some("closure"),
            closure_layout,
            Bytecode::from(vec![
                Instruction::load_this(BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );
        let module = Module::new(Some("method-this"), vec![entry, closure], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        let value = result.expect("method call should execute").return_value();
        assert!(
            value.as_object_handle().is_some(),
            "expected object receiver"
        );
    }

    #[test]
    fn prepare_direct_call_preserves_construct_flag_and_receiver() {
        let entry_layout = FrameLayout::new(0, 0, 0, 1).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(1, 0, 0, 0).expect("frame layout should be valid");
        let entry = Function::with_bytecode(Some("entry"), entry_layout, Bytecode::default());
        let helper = Function::with_bytecode(Some("helper"), helper_layout, Bytecode::default());
        let module = Module::new(Some("construct"), vec![entry, helper], FunctionIndex(0))
            .expect("module should be valid");
        let caller_function = module.function(FunctionIndex(0)).expect("entry must exist");
        let callee_function = module
            .function(FunctionIndex(1))
            .expect("helper must exist");
        let mut caller_activation = Activation::new(
            FunctionIndex(0),
            caller_function.frame_layout().register_count(),
        );
        let mut runtime = RuntimeState::new();
        let receiver = runtime.objects.alloc_object();
        caller_activation
            .write_bytecode_register(
                caller_function,
                BytecodeRegister::new(0).index(),
                RegisterValue::from_object_handle(receiver.0),
            )
            .expect("caller receiver register should exist");

        let callee_activation = Interpreter::prepare_direct_call(
            &module,
            caller_function,
            &caller_activation,
            0,
            DirectCall::new_with_receiver(
                FunctionIndex(1),
                0,
                FrameFlags::new(true, true, false),
                BytecodeRegister::new(0),
            ),
        )
        .expect("direct call setup should succeed");

        assert!(callee_activation.metadata().flags().is_construct());
        assert!(callee_activation.metadata().flags().has_receiver());
        assert_eq!(
            callee_activation
                .receiver(callee_function)
                .expect("callee receiver must exist"),
            RegisterValue::from_object_handle(receiver.0)
        );
    }

    #[test]
    fn interpreter_executes_closure_with_upvalue_updates() {
        let entry_layout = FrameLayout::new(0, 0, 0, 6).expect("frame layout should be valid");
        let closure_layout = FrameLayout::new(0, 1, 0, 4).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 1),
                Instruction::new_closure(BytecodeRegister::new(1), BytecodeRegister::new(0)),
                Instruction::load_i32(BytecodeRegister::new(2), 41),
                Instruction::call_closure(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(2),
                ),
                Instruction::load_i32(BytecodeRegister::new(4), 1),
                Instruction::call_closure(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(4),
                ),
                Instruction::ret(BytecodeRegister::new(5)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    ClosureTable::new(vec![
                        None,
                        Some(ClosureTemplate::new(FunctionIndex(1), 1)),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ]),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new(1, FrameFlags::empty()))),
                        None,
                        Some(CallSite::Closure(ClosureCall::new(1, FrameFlags::empty()))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let closure = Function::with_bytecode(
            Some("closure"),
            closure_layout,
            Bytecode::from(vec![
                Instruction::get_upvalue(BytecodeRegister::new(1), UpvalueId(0)),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                ),
                Instruction::set_upvalue(BytecodeRegister::new(2), UpvalueId(0)),
                Instruction::get_upvalue(BytecodeRegister::new(3), UpvalueId(0)),
                Instruction::ret(BytecodeRegister::new(3)),
            ]),
        );
        let module = Module::new(Some("closure"), vec![entry, closure], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(43))
        );
    }
}
