//! Interpreter entry points for the new VM.

use core::any::Any;
use core::fmt;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::bytecode::{BytecodeRegister, Instruction, Opcode, ProgramCounter};
use crate::call::{ClosureCall, DirectCall};
use crate::closure::{CaptureDescriptor, ClosureTemplate, UpvalueId};
use crate::descriptors::{NativeSlotKind, VmNativeCallError};
use crate::feedback::{FeedbackKind, FeedbackSlotId};
use crate::float::FloatId;
use crate::frame::{FrameFlags, FrameMetadata, RegisterIndex};
use crate::host::{HostFunctionId, NativeFunctionRegistry};
use crate::intrinsics::{
    VmIntrinsics, WellKnownSymbol, box_boolean_object, box_number_object, box_symbol_object,
};
use crate::module::{Function, FunctionIndex, Module};
use crate::object::{
    ClosureFlags as ObjectClosureFlags, HeapValueKind, ObjectError, ObjectHandle, ObjectHeap,
    PropertyAttributes, PropertyInlineCache, PropertyLookup, PropertyValue,
};
use crate::payload::{NativePayloadError, NativePayloadRegistry, VmTrace, VmValueTracer};
use crate::property::{PropertyNameId, PropertyNameRegistry};
use crate::string::StringId;
use crate::value::{RegisterValue, ValueError};

const STRING_DATA_SLOT: &str = "__otter_string_data__";
const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const BOOLEAN_DATA_SLOT: &str = "__otter_boolean_data__";

/// Errors produced by the new interpreter.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpreterError {
    /// The bytecode referenced a register outside the current frame layout.
    RegisterOutOfBounds,
    /// The interpreter reached the end of bytecode without an explicit return.
    UnexpectedEndOfBytecode,
    /// A branch jumped outside the valid bytecode range.
    InvalidJumpTarget,
    /// A constant table index was out of bounds.
    InvalidConstant,
    /// Execution was interrupted by an external signal (e.g. timeout watchdog).
    Interrupted,
    /// A TypeError was thrown at runtime.
    TypeError(Box<str>),
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
            Self::InvalidConstant => f.write_str("constant table index is out of bounds"),
            Self::Interrupted => f.write_str("execution interrupted"),
            Self::TypeError(msg) => write!(f, "TypeError: {msg}"),
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
            ObjectError::InvalidArrayLength => Self::NativeCall("invalid array length".into()),
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
    construct_new_target: Option<ObjectHandle>,
    pending_exception: Option<RegisterValue>,
    pc: ProgramCounter,
    registers: Box<[RegisterValue]>,
    open_upvalues: Box<[Option<ObjectHandle>]>,
    written_registers: Vec<RegisterIndex>,
    /// ES2024 §10.4.4 — Overflow arguments beyond formal parameter count.
    /// Stored separately from the register file to avoid polluting the frame layout.
    /// Used by `CreateArguments` to populate the arguments exotic object.
    overflow_args: Vec<RegisterValue>,
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

    /// Saves the entire register window as a boxed slice for generator suspension.
    pub fn save_registers(&self) -> Box<[RegisterValue]> {
        self.registers.clone()
    }

    /// Restores a previously saved register window into this activation.
    pub fn restore_registers(&mut self, saved: &[RegisterValue]) {
        let copy_len = saved.len().min(self.registers.len());
        self.registers[..copy_len].copy_from_slice(&saved[..copy_len]);
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
                self.written_registers.push(index);
                Ok(())
            }
            None => Err(InterpreterError::RegisterOutOfBounds),
        }
    }

    fn begin_step(&mut self) {
        self.written_registers.clear();
    }

    fn sync_written_open_upvalues(
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

    fn refresh_open_upvalues_from_cells(
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

    fn ensure_open_upvalue(
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

    fn capture_bytecode_register_upvalue(
        &mut self,
        function: &Function,
        runtime: &mut RuntimeState,
        register: BytecodeRegister,
    ) -> Result<ObjectHandle, InterpreterError> {
        let absolute = self.resolve_bytecode_register(function, register.index())?;
        self.ensure_open_upvalue(absolute, runtime)
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

#[derive(Debug, Clone, PartialEq)]
enum StepOutcome {
    Continue,
    Return(RegisterValue),
    Throw(RegisterValue),
    /// The interpreter should suspend at an `await` on a pending promise.
    /// The caller captures the frame state and enqueues a resume job.
    Suspend {
        /// The promise being awaited.
        awaited_promise: ObjectHandle,
        /// The register where the await result should be written on resume.
        resume_register: crate::frame::RegisterIndex,
    },
    /// The generator should yield a value and suspend.
    GeneratorYield {
        /// The value being yielded.
        yielded_value: RegisterValue,
        /// The register where the sent value should be written on resume.
        resume_register: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Completion {
    Return(RegisterValue),
    Throw(RegisterValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToPrimitiveHint {
    String,
    Number,
}

/// Shared execution runtime for one interpreter/JIT run.
pub struct RuntimeState {
    intrinsics: VmIntrinsics,
    objects: ObjectHeap,
    property_names: PropertyNameRegistry,
    native_functions: NativeFunctionRegistry,
    native_payloads: NativePayloadRegistry,
    microtasks: crate::microtask::MicrotaskQueue,
    timers: crate::event_loop::TimerRegistry,
    console_backend: Box<dyn crate::console::ConsoleBackend>,
    current_module: Option<Module>,
    native_call_construct_stack: Vec<bool>,
    next_symbol_id: u32,
    symbol_descriptions: BTreeMap<u32, Option<Box<str>>>,
    global_symbol_registry: BTreeMap<Box<str>, u32>,
    global_symbol_registry_reverse: BTreeMap<u32, Box<str>>,
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
        let mut symbol_descriptions = BTreeMap::new();
        for &symbol in intrinsics.well_known_symbols() {
            symbol_descriptions.insert(symbol.stable_id(), Some(symbol.description().into()));
        }

        Self {
            intrinsics,
            objects,
            property_names,
            native_functions,
            native_payloads: NativePayloadRegistry::new(),
            microtasks: crate::microtask::MicrotaskQueue::new(),
            timers: crate::event_loop::TimerRegistry::new(),
            console_backend: Box::new(crate::console::StdioConsoleBackend),
            current_module: None,
            native_call_construct_stack: Vec::new(),
            next_symbol_id: WellKnownSymbol::Unscopables.stable_id() + 1,
            symbol_descriptions,
            global_symbol_registry: BTreeMap::new(),
            global_symbol_registry_reverse: BTreeMap::new(),
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

    /// Returns `true` when the active native callback was entered via
    /// [[Construct]].
    #[must_use]
    pub fn is_current_native_construct_call(&self) -> bool {
        self.native_call_construct_stack
            .last()
            .copied()
            .unwrap_or(false)
    }

    /// Creates a property key iterator (for..in) from an object and its prototype chain.
    pub fn alloc_property_iterator(
        &mut self,
        object: ObjectHandle,
    ) -> Result<ObjectHandle, ObjectError> {
        self.objects
            .alloc_property_iterator(object, &self.property_names)
    }

    /// Creates an empty property iterator (for null/undefined/primitives in for..in).
    pub fn alloc_empty_property_iterator(&mut self) -> Result<ObjectHandle, ObjectError> {
        self.objects.alloc_empty_property_iterator()
    }

    /// Interns one property name into the runtime-wide registry.
    pub fn intern_property_name(&mut self, name: &str) -> PropertyNameId {
        self.property_names.intern(name)
    }

    /// Interns one symbol-keyed property into the runtime-wide registry.
    pub fn intern_symbol_property_name(&mut self, symbol_id: u32) -> PropertyNameId {
        self.property_names.intern_symbol(symbol_id)
    }

    /// Returns own property keys using the runtime-wide property-name registry.
    pub fn own_property_keys(
        &mut self,
        object: ObjectHandle,
    ) -> Result<Vec<PropertyNameId>, ObjectError> {
        let mut keys = self
            .objects
            .own_keys_with_registry(object, &mut self.property_names)?;
        keys.retain(|key| !self.is_hidden_internal_property(*key));

        let Some(string_handle) = self.string_exotic_value_handle(object)? else {
            return Ok(keys);
        };
        if string_handle == object {
            return Ok(keys);
        }

        let Some(string) = self.objects.string_value(string_handle)? else {
            return Ok(keys);
        };
        let length = string.chars().count();
        let mut result = Vec::with_capacity(length.saturating_add(1).saturating_add(keys.len()));
        for index in 0..length {
            result.push(self.property_names.intern(&index.to_string()));
        }
        result.push(self.property_names.intern("length"));
        result.extend(
            keys.into_iter()
                .filter(|key| !self.is_string_exotic_public_key(*key, length)),
        );
        Ok(result)
    }

    /// Returns an own property descriptor without prototype traversal.
    pub fn own_property_descriptor(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        if self.is_hidden_internal_property(property) {
            return Ok(None);
        }
        if let Some(descriptor) = self.string_exotic_own_property(object, property)? {
            return Ok(Some(descriptor));
        }
        self.objects
            .own_property_descriptor(object, property, &self.property_names)
    }

    /// Returns enumerable own property keys in spec-visible enumeration order.
    pub fn enumerable_own_property_keys(
        &mut self,
        object: ObjectHandle,
    ) -> Result<Vec<PropertyNameId>, VmNativeCallError> {
        let keys = self.own_property_keys(object).map_err(|error| {
            VmNativeCallError::Internal(format!("enumerable own keys failed: {error:?}").into())
        })?;
        let mut enumerable = Vec::with_capacity(keys.len());
        for key in keys {
            if self.property_names.is_symbol(key) {
                continue;
            }
            let Some(descriptor) = self.own_property_descriptor(object, key).map_err(|error| {
                VmNativeCallError::Internal(
                    format!("enumerable own descriptor failed: {error:?}").into(),
                )
            })?
            else {
                continue;
            };
            if descriptor.attributes().enumerable() {
                enumerable.push(key);
            }
        }
        Ok(enumerable)
    }

    /// Returns one own property value using the object itself as `receiver`.
    pub fn own_property_value(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<RegisterValue, VmNativeCallError> {
        self.ordinary_get(
            object,
            property,
            RegisterValue::from_object_handle(object.0),
        )
    }

    /// Returns a named property lookup using the runtime-wide property registry.
    pub fn property_lookup(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyLookup>, ObjectError> {
        if self.is_hidden_internal_property(property) {
            return Ok(None);
        }
        if let Some(descriptor) = self.string_exotic_own_property(object, property)? {
            return Ok(Some(PropertyLookup::new(object, descriptor, None)));
        }
        self.objects
            .get_property_with_registry(object, property, &self.property_names)
    }

    /// Returns whether a named property exists on an object or its prototype chain.
    pub fn has_property(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, ObjectError> {
        Ok(self.property_lookup(object, property)?.is_some())
    }

    /// Writes a named property using the runtime-wide property-name registry.
    pub fn set_named_property(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<PropertyInlineCache, InterpreterError> {
        match self
            .objects
            .set_property_with_registry(object, property, value, &self.property_names)
        {
            Ok(cache) => Ok(cache),
            Err(ObjectError::InvalidArrayLength) => Err(self.invalid_array_length_error()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn get_array_index_value(
        &mut self,
        object: ObjectHandle,
        index: usize,
    ) -> Result<Option<RegisterValue>, VmNativeCallError> {
        let property = self.intern_property_name(&index.to_string());
        match self.property_lookup(object, property).map_err(|error| {
            VmNativeCallError::Internal(format!("array index lookup failed: {error:?}").into())
        })? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { value, .. } => Ok(Some(value)),
                PropertyValue::Accessor { getter, .. } => self
                    .call_callable_for_accessor(
                        getter,
                        RegisterValue::from_object_handle(object.0),
                        &[],
                    )
                    .map(Some)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    }),
            },
            None => Ok(None),
        }
    }

    pub fn iterator_next(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<crate::object::IteratorStep, InterpreterError> {
        use crate::object::{ArrayIteratorKind, ObjectError};

        // Check if this is a values-kind array iterator (fast path) or string iterator.
        // Non-values array iterators, Map/Set iterators return InvalidKind to use the
        // protocol-based slow path via .next().
        let kind_check = self.objects.array_iterator_kind(handle);
        match kind_check {
            Ok(ArrayIteratorKind::Keys | ArrayIteratorKind::Entries) => {
                return Err(InterpreterError::InvalidHeapValueKind);
            }
            Err(ObjectError::InvalidKind) => {
                // Not an ArrayIterator — check if string/other internal iterator.
                if matches!(
                    self.objects.kind(handle),
                    Ok(crate::object::HeapValueKind::MapIterator
                        | crate::object::HeapValueKind::SetIterator)
                ) {
                    return Err(InterpreterError::InvalidHeapValueKind);
                }
            }
            _ => {} // Values kind — continue with fast path
        }

        let cursor = self.objects.iterator_cursor(handle)?;
        if cursor.closed() {
            return Ok(crate::object::IteratorStep::done());
        }

        let step = if cursor.is_array() {
            match self.objects.array_length(cursor.iterable())? {
                Some(length) if cursor.next_index() < length => {
                    let value =
                        match self.get_array_index_value(cursor.iterable(), cursor.next_index()) {
                            Ok(value) => value,
                            Err(VmNativeCallError::Thrown(value)) => {
                                return Err(InterpreterError::UncaughtThrow(value));
                            }
                            Err(VmNativeCallError::Internal(message)) => {
                                return Err(InterpreterError::NativeCall(message));
                            }
                        };
                    match value {
                        Some(value) => crate::object::IteratorStep::yield_value(value),
                        None => {
                            crate::object::IteratorStep::yield_value(RegisterValue::undefined())
                        }
                    }
                }
                _ => crate::object::IteratorStep::done(),
            }
        } else {
            match self
                .objects
                .get_index(cursor.iterable(), cursor.next_index())?
            {
                Some(value) => crate::object::IteratorStep::yield_value(value),
                None => crate::object::IteratorStep::done(),
            }
        };

        self.objects
            .advance_iterator_cursor(handle, step.is_done())?;
        Ok(step)
    }

    fn enter_module(&mut self, module: &Module) -> Option<Module> {
        let previous = self.current_module.clone();
        self.current_module = Some(module.clone());
        previous
    }

    fn restore_module(&mut self, previous: Option<Module>) {
        self.current_module = previous;
    }

    fn call_callable_for_accessor(
        &mut self,
        callable: Option<ObjectHandle>,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(callable) = callable else {
            return Ok(RegisterValue::undefined());
        };

        if let Ok(HeapValueKind::BoundFunction) = self.objects.kind(callable) {
            let (target, bound_this, bound_args) = self.objects.bound_function_parts(callable)?;
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return self.call_callable_for_accessor(Some(target), bound_this, &full_args);
        }

        let Some(module) = self.current_module.clone() else {
            return self
                .call_host_function(Some(callable), receiver, arguments)
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                });
        };

        Interpreter::call_function(self, &module, callable, receiver, arguments)
    }

    fn string_exotic_own_property(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        let Some(string_handle) = self.string_exotic_value_handle(object)? else {
            return Ok(None);
        };
        let Some(string) = self.objects.string_value(string_handle)? else {
            return Ok(None);
        };
        let Some(property_name) = self.property_names.get(property) else {
            return Ok(None);
        };

        if property_name == "length" {
            return Ok(Some(PropertyValue::data_with_attrs(
                RegisterValue::from_i32(i32::try_from(string.chars().count()).unwrap_or(i32::MAX)),
                PropertyAttributes::from_flags(false, false, false),
            )));
        }

        let Some(index) = canonical_string_exotic_index(property_name) else {
            return Ok(None);
        };
        let Some(character) = string.chars().nth(index) else {
            return Ok(None);
        };

        let character = self.alloc_string(character.to_string());
        Ok(Some(PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(character.0),
            PropertyAttributes::from_flags(false, true, false),
        )))
    }

    fn string_exotic_value_handle(
        &mut self,
        object: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        if self.objects.string_value(object)?.is_some() {
            return Ok(Some(object));
        }

        let backing = self.intern_property_name(STRING_DATA_SLOT);
        let Some(lookup) = self.objects.get_property(object, backing)? else {
            return Ok(None);
        };
        if lookup.owner() != object {
            return Ok(None);
        }
        let PropertyValue::Data { value, .. } = lookup.value() else {
            return Ok(None);
        };
        let Some(inner) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(None);
        };
        if self.objects.string_value(inner)?.is_some() {
            return Ok(Some(inner));
        }
        Ok(None)
    }

    fn is_hidden_internal_property(&self, property: PropertyNameId) -> bool {
        matches!(
            self.property_names.get(property),
            Some(STRING_DATA_SLOT | NUMBER_DATA_SLOT | BOOLEAN_DATA_SLOT)
        )
    }

    fn is_string_exotic_public_key(&self, property: PropertyNameId, length: usize) -> bool {
        let Some(name) = self.property_names.get(property) else {
            return false;
        };
        if name == "length" {
            return true;
        }
        canonical_string_exotic_index(name).is_some_and(|index| index < length)
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

    /// Returns the runtime-owned native payload registry.
    #[must_use]
    pub fn native_payloads(&self) -> &NativePayloadRegistry {
        &self.native_payloads
    }

    /// Returns the mutable runtime-owned native payload registry.
    pub fn native_payloads_mut(&mut self) -> &mut NativePayloadRegistry {
        &mut self.native_payloads
    }

    /// Registers one host-callable native function in the runtime registry.
    pub fn register_native_function(
        &mut self,
        descriptor: crate::descriptors::NativeFunctionDescriptor,
    ) -> HostFunctionId {
        self.native_functions.register(descriptor)
    }

    /// Returns the microtask queue.
    #[must_use]
    pub fn microtasks(&self) -> &crate::microtask::MicrotaskQueue {
        &self.microtasks
    }

    /// Returns the mutable microtask queue.
    pub fn microtasks_mut(&mut self) -> &mut crate::microtask::MicrotaskQueue {
        &mut self.microtasks
    }

    /// Returns the console backend.
    pub fn console(&self) -> &dyn crate::console::ConsoleBackend {
        self.console_backend.as_ref()
    }

    /// Replaces the console backend. Used by embedders to route output.
    pub fn set_console_backend(&mut self, backend: Box<dyn crate::console::ConsoleBackend>) {
        self.console_backend = backend;
    }

    /// Returns the timer registry.
    #[must_use]
    pub fn timers(&self) -> &crate::event_loop::TimerRegistry {
        &self.timers
    }

    /// Returns the mutable timer registry.
    pub fn timers_mut(&mut self) -> &mut crate::event_loop::TimerRegistry {
        &mut self.timers
    }

    /// Schedules a one-shot timer (setTimeout).
    pub fn schedule_timeout(
        &mut self,
        callback: ObjectHandle,
        delay: std::time::Duration,
    ) -> crate::event_loop_host::TimerId {
        self.timers
            .set_timeout(callback, RegisterValue::undefined(), delay)
    }

    /// Schedules a repeating timer (setInterval).
    pub fn schedule_interval(
        &mut self,
        callback: ObjectHandle,
        interval: std::time::Duration,
    ) -> crate::event_loop_host::TimerId {
        self.timers
            .set_interval(callback, RegisterValue::undefined(), interval)
    }

    /// Cancels a timer.
    pub fn clear_timer(&mut self, id: crate::event_loop_host::TimerId) {
        self.timers.clear(id);
    }

    /// GC safepoint — called at loop back-edges and function call boundaries.
    /// Collects roots from intrinsics and the provided register window,
    /// then triggers collection if memory pressure warrants it.
    pub fn gc_safepoint(&mut self, registers: &[RegisterValue]) {
        let mut roots = self.intrinsics.gc_root_handles();
        // Extract ObjectHandle roots from the current register window.
        for reg in registers {
            if let Some(handle) = reg.as_object_handle() {
                roots.push(ObjectHandle(handle));
            }
        }
        self.objects.maybe_collect_garbage(&roots);
    }

    /// Allocates one ordinary object with the runtime default prototype.
    pub fn alloc_object(&mut self) -> ObjectHandle {
        let prototype = self.intrinsics.object_prototype();
        let handle = self.objects.alloc_object();
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("ordinary object prototype should exist");
        handle
    }

    /// Allocates one ordinary object with an explicit prototype.
    pub fn alloc_object_with_prototype(&mut self, prototype: Option<ObjectHandle>) -> ObjectHandle {
        let handle = self.objects.alloc_object();
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit object prototype should be valid");
        handle
    }

    /// Allocates one ordinary object that carries a Rust-owned native payload.
    pub fn alloc_native_object<T>(&mut self, payload: T) -> ObjectHandle
    where
        T: VmTrace + Any,
    {
        let prototype = self.intrinsics.object_prototype();
        self.alloc_native_object_with_prototype(Some(prototype), payload)
    }

    /// Allocates one payload-bearing object with an explicit prototype.
    pub fn alloc_native_object_with_prototype<T>(
        &mut self,
        prototype: Option<ObjectHandle>,
        payload: T,
    ) -> ObjectHandle
    where
        T: VmTrace + Any,
    {
        let payload = self.native_payloads.insert(payload);
        let handle = self.objects.alloc_native_object(payload);
        self.objects
            .set_prototype(handle, prototype)
            .expect("explicit native object prototype should be valid");
        handle
    }

    /// Allocates one dense array with the runtime default prototype.
    pub fn alloc_array(&mut self) -> ObjectHandle {
        let prototype = self.intrinsics.array_prototype();
        let handle = self.objects.alloc_array();
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("array prototype should exist");
        handle
    }

    /// Allocates an array and populates it with initial elements.
    pub fn alloc_array_with_elements(&mut self, elements: &[RegisterValue]) -> ObjectHandle {
        let handle = self.alloc_array();
        for &elem in elements {
            self.objects
                .push_element(handle, elem)
                .expect("array push should succeed");
        }
        handle
    }

    /// Extracts elements from an array handle into a Vec of RegisterValues.
    pub fn array_to_args(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        self.objects
            .array_elements(handle)
            .map_err(|e| VmNativeCallError::Internal(format!("array_to_args failed: {e:?}").into()))
    }

    pub fn list_from_array_like(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Vec<RegisterValue>, VmNativeCallError> {
        let length_key = self.intern_property_name("length");
        let receiver = RegisterValue::from_object_handle(handle.0);
        let length_value = self.ordinary_get(handle, length_key, receiver)?;
        let length = usize::try_from(self.js_to_uint32(length_value).map_err(
            |error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                    VmNativeCallError::Internal(message)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            },
        )?)
        .unwrap_or(usize::MAX);

        let mut values = Vec::with_capacity(length);
        for index in 0..length {
            let property = self.intern_property_name(&index.to_string());
            let value = self.ordinary_get(handle, property, receiver)?;
            values.push(value);
        }
        Ok(values)
    }

    /// Allocates one string object with the runtime default prototype.
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let prototype = self.intrinsics.string_prototype();
        let handle = self.objects.alloc_string(value);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        handle
    }

    /// Allocates one fresh symbol primitive with a VM-wide stable identifier.
    pub fn alloc_symbol(&mut self) -> RegisterValue {
        self.alloc_symbol_with_description(None)
    }

    /// Allocates one fresh symbol primitive and records its optional description.
    pub fn alloc_symbol_with_description(
        &mut self,
        description: Option<Box<str>>,
    ) -> RegisterValue {
        let symbol_id = self.next_symbol_id;
        self.next_symbol_id = self
            .next_symbol_id
            .checked_add(1)
            .expect("symbol identifier space exhausted");
        self.symbol_descriptions.insert(symbol_id, description);
        RegisterValue::from_symbol_id(symbol_id)
    }

    /// Returns the recorded description for a symbol value, if any.
    #[must_use]
    pub fn symbol_description(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.symbol_descriptions
            .get(&symbol_id)
            .and_then(|description| description.as_deref())
    }

    /// Interns a global-registry symbol key and returns the canonical symbol value.
    pub fn intern_global_symbol(&mut self, key: Box<str>) -> RegisterValue {
        if let Some(&symbol_id) = self.global_symbol_registry.get(key.as_ref()) {
            return RegisterValue::from_symbol_id(symbol_id);
        }

        let symbol = self.alloc_symbol_with_description(Some(key.clone()));
        let symbol_id = symbol
            .as_symbol_id()
            .expect("allocated symbol should expose a symbol id");
        self.global_symbol_registry.insert(key.clone(), symbol_id);
        self.global_symbol_registry_reverse.insert(symbol_id, key);
        symbol
    }

    /// Returns the registry key for a symbol value, if it was created via `Symbol.for`.
    #[must_use]
    pub fn symbol_registry_key(&self, value: RegisterValue) -> Option<&str> {
        let symbol_id = value.as_symbol_id()?;
        self.global_symbol_registry_reverse
            .get(&symbol_id)
            .map(Box::as_ref)
    }

    /// Allocates a new symbol from a JS-visible description value.
    pub fn create_symbol_from_value(
        &mut self,
        description: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        if description == RegisterValue::undefined() {
            return Ok(self.alloc_symbol_with_description(None));
        }
        let description = self.coerce_symbol_string(description)?;
        Ok(self.alloc_symbol_with_description(Some(description)))
    }

    /// Resolves `Symbol.for(key)` using the runtime-wide global symbol registry.
    pub fn symbol_for_value(
        &mut self,
        key: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let key = self.coerce_symbol_string(key)?;
        Ok(self.intern_global_symbol(key))
    }

    fn coerce_symbol_string(&mut self, value: RegisterValue) -> Result<Box<str>, InterpreterError> {
        self.js_to_string(value)
    }

    /// Allocates one host-callable function with the runtime default prototype.
    pub fn alloc_host_function(&mut self, function: HostFunctionId) -> ObjectHandle {
        let prototype = self.intrinsics.function_prototype();
        let handle = self.objects.alloc_host_function(function);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        handle
    }

    /// Registers a native function and installs it as a property on the global object.
    ///
    /// This is the primary API for embedders to inject host-provided globals
    /// (e.g., `print`, `$DONE`, `$262`) into the runtime.
    pub fn install_native_global(
        &mut self,
        descriptor: crate::descriptors::NativeFunctionDescriptor,
    ) -> ObjectHandle {
        let host_fn = self.native_functions.register(descriptor);
        let handle = self.alloc_host_function(host_fn);
        let global = self.intrinsics.global_object();
        let prop = self.property_names.intern(
            self.native_functions
                .get(host_fn)
                .expect("just registered")
                .js_name(),
        );
        self.objects
            .set_property(global, prop, RegisterValue::from_object_handle(handle.0))
            .expect("global property installation should succeed");
        handle
    }

    /// Installs a value property on the global object.
    pub fn install_global_value(&mut self, name: &str, value: RegisterValue) {
        let global = self.intrinsics.global_object();
        let prop = self.property_names.intern(name);
        self.objects
            .set_property(global, prop, value)
            .expect("global property installation should succeed");
    }

    /// Allocates one bytecode closure with the runtime default function prototype.
    pub fn alloc_closure(
        &mut self,
        callee: FunctionIndex,
        upvalues: Vec<ObjectHandle>,
        flags: ObjectClosureFlags,
    ) -> ObjectHandle {
        let prototype = self.intrinsics.function_prototype();
        let module = self
            .current_module
            .clone()
            .expect("closure allocation requires active module context");
        let handle = self.objects.alloc_closure(module, callee, upvalues, flags);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
        let closure_length = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .map(|function| function.length())
            .unwrap_or(0);
        let closure_name = self
            .current_module
            .as_ref()
            .and_then(|module| module.function(callee))
            .and_then(|function| function.name())
            .unwrap_or("")
            .to_string();
        let length_property = self.intern_property_name("length");
        self.objects
            .define_own_property(
                handle,
                length_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_i32(i32::from(closure_length)),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure length should install");
        let name_property = self.intern_property_name("name");
        let name_handle = self.alloc_string(closure_name);
        self.objects
            .define_own_property(
                handle,
                name_property,
                PropertyValue::data_with_attrs(
                    RegisterValue::from_object_handle(name_handle.0),
                    PropertyAttributes::function_length(),
                ),
            )
            .expect("closure name should install");
        // Only constructable closures get a .prototype property (§10.2.6).
        if flags.is_constructable() {
            let prototype_property = self.intern_property_name("prototype");
            let constructor_property = self.intern_property_name("constructor");
            let instance_prototype = self.alloc_object();
            self.objects
                .define_own_property(
                    handle,
                    prototype_property,
                    PropertyValue::data_with_attrs(
                        RegisterValue::from_object_handle(instance_prototype.0),
                        PropertyAttributes::function_prototype(),
                    ),
                )
                .expect("closure prototype object should install");
            self.objects
                .define_own_property(
                    instance_prototype,
                    constructor_property,
                    PropertyValue::data_with_attrs(
                        RegisterValue::from_object_handle(handle.0),
                        PropertyAttributes::constructor_link(),
                    ),
                )
                .expect("closure prototype.constructor should install");
        }
        handle
    }

    /// ES2024 §7.2.4 IsConstructor — checks if a value has `[[Construct]]`.
    pub fn is_constructible(&self, handle: ObjectHandle) -> bool {
        match self.objects.kind(handle) {
            Ok(HeapValueKind::HostFunction) => {
                // Host functions are constructors only if registered with Constructor slot kind.
                if let Ok(Some(host_fn_id)) = self.objects.host_function(handle) {
                    self.native_functions.get(host_fn_id).is_some_and(|desc| {
                        desc.slot_kind() == crate::descriptors::NativeSlotKind::Constructor
                    })
                } else {
                    false
                }
            }
            Ok(HeapValueKind::Closure) => self
                .objects
                .closure_flags(handle)
                .is_ok_and(|f| f.is_constructable()),
            Ok(HeapValueKind::BoundFunction) => self
                .objects
                .bound_function_parts(handle)
                .is_ok_and(|(target, _, _)| self.is_constructible(target)),
            _ => false,
        }
    }

    /// Resolves one native payload from a payload-bearing object.
    pub fn native_payload<T>(&self, handle: ObjectHandle) -> Result<&T, NativePayloadError>
    where
        T: Any,
    {
        let payload = self
            .objects
            .native_payload_id(handle)?
            .ok_or(NativePayloadError::MissingPayload)?;
        self.native_payloads.get::<T>(payload)
    }

    /// Resolves one mutable native payload from a payload-bearing object.
    pub fn native_payload_mut<T>(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<&mut T, NativePayloadError>
    where
        T: Any,
    {
        let payload = self
            .objects
            .native_payload_id(handle)?
            .ok_or(NativePayloadError::MissingPayload)?;
        self.native_payloads.get_mut::<T>(payload)
    }

    /// Resolves one native payload from a JS-visible receiver value.
    pub fn native_payload_from_value<T>(
        &self,
        value: &RegisterValue,
    ) -> Result<&T, NativePayloadError>
    where
        T: Any,
    {
        let handle = value
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(NativePayloadError::ExpectedObjectValue)?;
        self.native_payload::<T>(handle)
    }

    /// Resolves one mutable native payload from a JS-visible receiver value.
    pub fn native_payload_mut_from_value<T>(
        &mut self,
        value: &RegisterValue,
    ) -> Result<&mut T, NativePayloadError>
    where
        T: Any,
    {
        let handle = value
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(NativePayloadError::ExpectedObjectValue)?;
        self.native_payload_mut::<T>(handle)
    }

    /// Traces GC-visible values stored inside native payload-bearing objects.
    pub fn trace_native_payload_roots(
        &self,
        tracer: &mut dyn VmValueTracer,
    ) -> Result<(), NativePayloadError> {
        let mut result = Ok(());
        self.objects
            .trace_native_payload_links(&mut |_handle, payload| {
                if result.is_ok() {
                    result = self.native_payloads.trace_payload(payload, tracer);
                }
            });
        result
    }

    /// Converts a JS-visible property key value into the runtime property-name id.
    pub fn property_name_from_value(
        &mut self,
        value: RegisterValue,
    ) -> Result<PropertyNameId, VmNativeCallError> {
        crate::abstract_ops::to_property_key(self, value)
    }

    /// Executes ordinary named-property `[[Get]]` with an explicit receiver.
    pub fn ordinary_get(
        &mut self,
        target: ObjectHandle,
        property: PropertyNameId,
        receiver: RegisterValue,
    ) -> Result<RegisterValue, VmNativeCallError> {
        match self.property_lookup(target, property).map_err(|error| {
            VmNativeCallError::Internal(format!("ordinary get failed: {error:?}").into())
        })? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { value, .. } => Ok(value),
                PropertyValue::Accessor { getter, .. } => self
                    .call_callable_for_accessor(getter, receiver, &[])
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    }),
            },
            None => Ok(RegisterValue::undefined()),
        }
    }

    /// Executes ordinary named-property `[[Set]]` with an explicit receiver.
    pub fn ordinary_set(
        &mut self,
        target: ObjectHandle,
        property: PropertyNameId,
        receiver: RegisterValue,
        value: RegisterValue,
    ) -> Result<bool, VmNativeCallError> {
        match self.property_lookup(target, property).map_err(|error| {
            VmNativeCallError::Internal(format!("ordinary set failed: {error:?}").into())
        })? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { attributes, .. } => {
                    let Some(receiver_handle) =
                        self.non_string_object_handle(receiver).map_err(|error| {
                            VmNativeCallError::Internal(
                                format!("ordinary set receiver check failed: {error:?}").into(),
                            )
                        })?
                    else {
                        return Ok(false);
                    };

                    if !attributes.writable() {
                        return Ok(false);
                    }

                    if lookup.owner() == receiver_handle {
                        if let Some(cache) = lookup.cache() {
                            let updated = self
                                .objects
                                .set_cached(receiver_handle, property, value, cache)
                                .map_err(|error| {
                                    VmNativeCallError::Internal(
                                        format!("ordinary set receiver update failed: {error:?}")
                                            .into(),
                                    )
                                })?;
                            if !updated {
                                self.objects
                                    .set_property(receiver_handle, property, value)
                                    .map_err(|error| {
                                        VmNativeCallError::Internal(
                                            format!(
                                                "ordinary set receiver fallback failed: {error:?}"
                                            )
                                            .into(),
                                        )
                                    })?;
                            }
                            return Ok(true);
                        }

                        return self.ordinary_set_on_receiver(receiver_handle, property, value);
                    }

                    self.ordinary_set_on_receiver(receiver_handle, property, value)
                }
                PropertyValue::Accessor { setter, .. } => {
                    let _ = self
                        .call_callable_for_accessor(setter, receiver, &[value])
                        .map_err(|error| match error {
                            InterpreterError::UncaughtThrow(value) => {
                                VmNativeCallError::Thrown(value)
                            }
                            InterpreterError::NativeCall(message)
                            | InterpreterError::TypeError(message) => {
                                VmNativeCallError::Internal(message)
                            }
                            other => VmNativeCallError::Internal(format!("{other}").into()),
                        })?;
                    Ok(setter.is_some())
                }
            },
            None => {
                let Some(receiver_handle) =
                    self.non_string_object_handle(receiver).map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("ordinary set receiver create check failed: {error:?}").into(),
                        )
                    })?
                else {
                    return Ok(false);
                };
                self.ordinary_set_on_receiver(receiver_handle, property, value)
            }
        }
    }

    fn ordinary_set_on_receiver(
        &mut self,
        receiver_handle: ObjectHandle,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, VmNativeCallError> {
        match self
            .own_property_descriptor(receiver_handle, property)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("ordinary set receiver own-descriptor failed: {error:?}").into(),
                )
            })? {
            Some(PropertyValue::Data { attributes, .. }) => {
                if !attributes.writable() {
                    return Ok(false);
                }
                self.set_named_property(receiver_handle, property, value)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                self.receiver_data_property_matches(receiver_handle, property, value)
            }
            Some(PropertyValue::Accessor { setter, .. }) => {
                let _ = self
                    .call_callable_for_accessor(
                        setter,
                        RegisterValue::from_object_handle(receiver_handle.0),
                        &[value],
                    )
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                Ok(setter.is_some())
            }
            None => {
                if !self
                    .objects
                    .is_extensible(receiver_handle)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("ordinary set receiver extensible check failed: {error:?}")
                                .into(),
                        )
                    })?
                {
                    return Ok(false);
                }
                self.set_named_property(receiver_handle, property, value)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                self.receiver_data_property_matches(receiver_handle, property, value)
            }
        }
    }

    fn receiver_data_property_matches(
        &mut self,
        receiver_handle: ObjectHandle,
        property: PropertyNameId,
        expected: RegisterValue,
    ) -> Result<bool, VmNativeCallError> {
        let descriptor = self
            .own_property_descriptor(receiver_handle, property)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("ordinary set receiver verification failed: {error:?}").into(),
                )
            })?;
        match descriptor {
            Some(PropertyValue::Data { value, .. }) => {
                self.objects.same_value(value, expected).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("ordinary set receiver SameValue failed: {error:?}").into(),
                    )
                })
            }
            _ => Ok(false),
        }
    }

    pub fn call_host_function(
        &mut self,
        callable: Option<ObjectHandle>,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, VmNativeCallError> {
        let Some(callable) = callable else {
            return Ok(RegisterValue::undefined());
        };

        // ES2024 §10.4.1.1 [[Call]] — resolve bound function chain.
        if let Ok(HeapValueKind::BoundFunction) = self.objects.kind(callable) {
            let (target, bound_this, bound_args) =
                self.objects.bound_function_parts(callable).map_err(|e| {
                    VmNativeCallError::Internal(format!("bound function resolution: {e:?}").into())
                })?;
            // Prepend bound_args to arguments.
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return self.call_host_function(Some(target), bound_this, &full_args);
        }

        // ES2024 §27.2.1.3 — Promise capability resolve/reject functions.
        if let Ok(HeapValueKind::PromiseCapabilityFunction) = self.objects.kind(callable) {
            let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
            Interpreter::invoke_promise_capability_function(self, callable, value)
                .map_err(|e| match e {
                    InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                })?;
            return Ok(RegisterValue::undefined());
        }

        // Promise combinator/finally/thunk dispatch.
        match self.objects.kind(callable) {
            Ok(HeapValueKind::PromiseCombinatorElement) => {
                let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
                return Interpreter::invoke_promise_combinator_element(self, callable, value)
                    .map_err(|e| match e {
                        InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    });
            }
            Ok(HeapValueKind::PromiseFinallyFunction) => {
                let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
                return Interpreter::invoke_promise_finally_function(self, callable, value)
                    .map_err(|e| match e {
                        InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    });
            }
            Ok(HeapValueKind::PromiseValueThunk) => {
                if let Some((v, k)) = self.objects.promise_value_thunk_info(callable) {
                    return match k {
                        crate::promise::PromiseFinallyKind::ThenFinally => Ok(v),
                        crate::promise::PromiseFinallyKind::CatchFinally => {
                            Err(VmNativeCallError::Thrown(v))
                        }
                    };
                }
            }
            _ => {}
        }

        // If it's a Closure (compiled JS function), dispatch through Interpreter::call_function.
        if let Ok(HeapValueKind::Closure) = self.objects.kind(callable) {
            // call_function ignores the module param for closures (gets it from the closure).
            // We need a Module reference, so extract from the closure itself.
            let module = self.objects.closure_module(callable).map_err(|e| {
                VmNativeCallError::Internal(format!("closure module lookup: {e:?}").into())
            })?;
            return Interpreter::call_function(self, &module, callable, receiver, arguments)
                .map_err(|e| match e {
                    InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                });
        }

        let host_function = self
            .objects
            .host_function(callable)
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("native callable lookup failed: {error:?}").into(),
                )
            })?
            .ok_or_else(|| {
                VmNativeCallError::Internal("native callable is not a host function".into())
            })?;
        let descriptor = self
            .native_functions
            .get(host_function)
            .cloned()
            .ok_or_else(|| {
                VmNativeCallError::Internal("host function descriptor is missing".into())
            })?;

        match (descriptor.callback())(&receiver, arguments, self) {
            Ok(value) => Ok(value),
            Err(VmNativeCallError::Thrown(value)) => Err(VmNativeCallError::Thrown(value)),
            Err(VmNativeCallError::Internal(message)) => Err(VmNativeCallError::Internal(message)),
        }
    }

    pub fn call_callable(
        &mut self,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, VmNativeCallError> {
        self.call_callable_for_accessor(Some(callable), receiver, arguments)
            .map_err(|error| match error {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                InterpreterError::NativeCall(message) | InterpreterError::TypeError(message) => {
                    VmNativeCallError::Internal(message)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            })
    }

    pub fn construct_callable(
        &mut self,
        target: ObjectHandle,
        arguments: &[RegisterValue],
        new_target: ObjectHandle,
    ) -> Result<RegisterValue, VmNativeCallError> {
        if !self.is_constructible(target) {
            let error = self
                .alloc_type_error("construct target is not constructible")
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct TypeError allocation failed: {error}").into(),
                    )
                })?;
            return Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ));
        }
        if !self.is_constructible(new_target) {
            let error = self
                .alloc_type_error("construct newTarget is not constructible")
                .map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct TypeError allocation failed: {error}").into(),
                    )
                })?;
            return Err(VmNativeCallError::Thrown(
                RegisterValue::from_object_handle(error.0),
            ));
        }
        let kind = self.objects.kind(target).map_err(|error| {
            VmNativeCallError::Internal(
                format!("construct target kind lookup failed: {error:?}").into(),
            )
        })?;
        let completion = match kind {
            HeapValueKind::BoundFunction => {
                let (bound_target, _, bound_args) =
                    self.objects.bound_function_parts(target).map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("construct bound function lookup failed: {error:?}").into(),
                        )
                    })?;
                let mut full_args = bound_args;
                full_args.extend_from_slice(arguments);
                let forwarded_new_target = if new_target == target {
                    bound_target
                } else {
                    new_target
                };
                return self.construct_callable(bound_target, &full_args, forwarded_new_target);
            }
            HeapValueKind::HostFunction => {
                let host_function = self
                    .objects
                    .host_function(target)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("construct host function lookup failed: {error:?}").into(),
                        )
                    })?
                    .ok_or_else(|| {
                        VmNativeCallError::Internal(
                            "construct target host function is missing".into(),
                        )
                    })?;
                let default_receiver = RegisterValue::from_object_handle(
                    Interpreter::allocate_construct_receiver(self, new_target)
                        .map_err(|error| match error {
                            InterpreterError::UncaughtThrow(value) => {
                                VmNativeCallError::Thrown(value)
                            }
                            other => VmNativeCallError::Internal(format!("{other}").into()),
                        })?
                        .0,
                );
                let completion = Interpreter::invoke_registered_host_function(
                    self,
                    host_function,
                    default_receiver,
                    arguments,
                    true,
                )
                .map_err(|error| match error {
                    InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                    other => VmNativeCallError::Internal(format!("{other}").into()),
                })?;
                Interpreter::apply_construct_return_override(completion, default_receiver)
            }
            HeapValueKind::Closure => {
                let module = self.objects.closure_module(target).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct closure module lookup failed: {error:?}").into(),
                    )
                })?;
                let callee_index = self.objects.closure_callee(target).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("construct closure callee lookup failed: {error:?}").into(),
                    )
                })?;
                let callee_function = module.function(callee_index).ok_or_else(|| {
                    VmNativeCallError::Internal("construct closure callee is missing".into())
                })?;
                let register_count = callee_function.frame_layout().register_count();
                let is_derived_constructor = callee_function.is_derived_constructor();
                let default_receiver = if is_derived_constructor {
                    RegisterValue::undefined()
                } else {
                    RegisterValue::from_object_handle(
                        Interpreter::allocate_construct_receiver(self, new_target)
                            .map_err(|error| match error {
                                InterpreterError::UncaughtThrow(value) => {
                                    VmNativeCallError::Thrown(value)
                                }
                                other => VmNativeCallError::Internal(format!("{other}").into()),
                            })?
                            .0,
                    )
                };
                let mut activation = Activation::with_context(
                    callee_index,
                    register_count,
                    FrameMetadata::new(
                        arguments.len() as RegisterIndex,
                        FrameFlags::new(true, true, false),
                    ),
                    Some(target),
                );
                activation.set_construct_new_target(Some(new_target));

                if callee_function.frame_layout().receiver_slot().is_some() {
                    activation
                        .set_receiver(callee_function, default_receiver)
                        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
                }

                let param_count = callee_function.frame_layout().parameter_count();
                for (index, &argument) in arguments.iter().take(param_count as usize).enumerate() {
                    let register = callee_function
                        .frame_layout()
                        .resolve_user_visible(index as u16)
                        .ok_or_else(|| {
                            VmNativeCallError::Internal(
                                "construct argument register resolution failed".into(),
                            )
                        })?;
                    activation
                        .set_register(register, argument)
                        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
                }
                if arguments.len() > param_count as usize {
                    activation.overflow_args = arguments[param_count as usize..].to_vec();
                }

                let completion = Interpreter::new()
                    .run_completion_with_runtime(&module, &mut activation, self)
                    .map_err(|error| match error {
                        InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                        InterpreterError::NativeCall(message)
                        | InterpreterError::TypeError(message) => {
                            VmNativeCallError::Internal(message)
                        }
                        other => VmNativeCallError::Internal(format!("{other}").into()),
                    })?;
                if is_derived_constructor {
                    match completion {
                        Completion::Return(value) if value.as_object_handle().is_some() => {
                            Completion::Return(value)
                        }
                        Completion::Return(value) if value != RegisterValue::undefined() => {
                            let error = self
                                .alloc_type_error(
                                    "Derived constructors may only return object or undefined values",
                                )
                                .map_err(|error| {
                                    VmNativeCallError::Internal(format!("{error}").into())
                                })?;
                            Completion::Throw(RegisterValue::from_object_handle(error.0))
                        }
                        Completion::Return(_) => {
                            let this_value =
                                if callee_function.frame_layout().receiver_slot().is_some() {
                                    activation.receiver(callee_function).map_err(|error| {
                                        VmNativeCallError::Internal(format!("{error}").into())
                                    })?
                                } else {
                                    RegisterValue::undefined()
                                };
                            if this_value.as_object_handle().is_some() {
                                Completion::Return(this_value)
                            } else {
                                let error = self
                                    .alloc_reference_error(
                                        "Must call super constructor in derived class before returning from derived constructor",
                                    )
                                    .map_err(|error| {
                                        VmNativeCallError::Internal(
                                            format!(
                                                "construct ReferenceError allocation failed: {error}"
                                            )
                                            .into(),
                                        )
                                    })?;
                                Completion::Throw(RegisterValue::from_object_handle(error.0))
                            }
                        }
                        Completion::Throw(value) => Completion::Throw(value),
                    }
                } else {
                    Interpreter::apply_construct_return_override(completion, default_receiver)
                }
            }
            _ => {
                return Err(VmNativeCallError::Internal(
                    "construct target is not callable".into(),
                ));
            }
        };

        match completion {
            Completion::Return(value) => Ok(value),
            Completion::Throw(value) => Err(VmNativeCallError::Thrown(value)),
        }
    }

    fn delete_named_property(
        &mut self,
        target: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<bool, InterpreterError> {
        self.objects
            .delete_property_with_registry(target, property, &self.property_names)
            .map_err(Into::into)
    }

    fn invalid_array_length_error(&mut self) -> InterpreterError {
        let prototype = self.intrinsics().range_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let message = self.alloc_string("Invalid array length");
        let message_prop = self.intern_property_name("message");
        self.objects
            .set_property(
                handle,
                message_prop,
                RegisterValue::from_object_handle(message.0),
            )
            .ok();
        InterpreterError::UncaughtThrow(RegisterValue::from_object_handle(handle.0))
    }

    fn own_data_property(
        &mut self,
        handle: ObjectHandle,
        slot_name: &str,
    ) -> Result<Option<RegisterValue>, InterpreterError> {
        let backing = self.intern_property_name(slot_name);
        let Some(lookup) = self.objects.get_property(handle, backing)? else {
            return Ok(None);
        };
        if lookup.owner() != handle {
            return Ok(None);
        }
        let PropertyValue::Data { value, .. } = lookup.value() else {
            return Ok(None);
        };
        Ok(Some(value))
    }

    fn string_wrapper_data(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Option<ObjectHandle>, InterpreterError> {
        Ok(self
            .own_data_property(handle, STRING_DATA_SLOT)?
            .and_then(|value| value.as_object_handle().map(ObjectHandle)))
    }

    fn js_loose_eq(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        if self.objects.strict_eq(lhs, rhs)? {
            return Ok(true);
        }
        if (lhs == RegisterValue::undefined() && rhs == RegisterValue::null())
            || (lhs == RegisterValue::null() && rhs == RegisterValue::undefined())
        {
            return Ok(true);
        }

        let coerced_lhs = self.coerce_loose_equality_primitive(lhs)?;
        let coerced_rhs = self.coerce_loose_equality_primitive(rhs)?;
        if coerced_lhs == coerced_rhs {
            return Ok(true);
        }
        if coerced_lhs != lhs || coerced_rhs != rhs {
            return self.js_loose_eq(coerced_lhs, coerced_rhs);
        }

        Ok(false)
    }

    fn non_string_object_handle(
        &self,
        value: RegisterValue,
    ) -> Result<Option<ObjectHandle>, ObjectError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(None);
        };
        if matches!(self.objects.kind(handle)?, HeapValueKind::String) {
            return Ok(None);
        }
        Ok(Some(handle))
    }

    fn computed_property_name(
        &mut self,
        key: RegisterValue,
    ) -> Result<PropertyNameId, InterpreterError> {
        self.property_name_from_value(key)
            .map_err(|error| match error {
                VmNativeCallError::Thrown(_) => {
                    InterpreterError::TypeError("property key coercion threw".into())
                }
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })
    }

    pub(crate) fn property_base_object_handle(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            return Err(InterpreterError::TypeError(
                "Cannot read properties of null or undefined".into(),
            ));
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return Ok(handle);
        }
        if let Some(boolean) = value.as_bool() {
            let object =
                box_boolean_object(RegisterValue::from_bool(boolean), self).map_err(|error| {
                    match error {
                        VmNativeCallError::Thrown(_) => {
                            InterpreterError::TypeError("boolean boxing threw".into())
                        }
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    }
                })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed boolean should return object handle"),
            ));
        }
        if let Some(number) = value.as_number() {
            let object =
                box_number_object(RegisterValue::from_number(number), self).map_err(|error| {
                    match error {
                        VmNativeCallError::Thrown(_) => {
                            InterpreterError::TypeError("number boxing threw".into())
                        }
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    }
                })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed number should return object handle"),
            ));
        }
        if value.is_symbol() {
            let object = box_symbol_object(value, self).map_err(|error| match error {
                VmNativeCallError::Thrown(_) => {
                    InterpreterError::TypeError("symbol boxing threw".into())
                }
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })?;
            return Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed symbol should return object handle"),
            ));
        }
        Err(InterpreterError::InvalidObjectValue)
    }

    pub(crate) fn property_set_target_handle(
        &mut self,
        value: RegisterValue,
    ) -> Result<ObjectHandle, InterpreterError> {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            return Err(InterpreterError::TypeError(
                "Cannot set properties of null or undefined".into(),
            ));
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return Ok(handle);
        }
        if value.as_bool().is_some() {
            return Ok(self.intrinsics.boolean_prototype());
        }
        if value.as_number().is_some() {
            return Ok(self.intrinsics.number_prototype());
        }
        if value.is_symbol() {
            return Ok(self.intrinsics.symbol_prototype());
        }
        Err(InterpreterError::InvalidObjectValue)
    }

    fn is_primitive_property_base(&self, value: RegisterValue) -> Result<bool, ObjectError> {
        if value.as_bool().is_some() || value.as_number().is_some() || value.is_symbol() {
            return Ok(true);
        }
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        Ok(matches!(self.objects.kind(handle)?, HeapValueKind::String))
    }

    fn ordinary_to_primitive(
        &mut self,
        value: RegisterValue,
        hint: ToPrimitiveHint,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };

        let method_names = match hint {
            ToPrimitiveHint::String => ["toString", "valueOf"],
            ToPrimitiveHint::Number => ["valueOf", "toString"],
        };

        for method_name in method_names {
            let property = self.intern_property_name(method_name);
            let method =
                self.ordinary_get(handle, property, value)
                    .map_err(|error| match error {
                        VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                        VmNativeCallError::Internal(message) => {
                            InterpreterError::NativeCall(message)
                        }
                    })?;
            let Some(callable) = method.as_object_handle().map(ObjectHandle) else {
                continue;
            };
            if !self.objects.is_callable(callable) {
                continue;
            }

            let result = self
                .call_callable(callable, value, &[])
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;
            if self.non_string_object_handle(result)?.is_none() {
                return Ok(result);
            }
        }

        Err(InterpreterError::TypeError(
            "Cannot convert object to primitive value".into(),
        ))
    }

    pub(crate) fn js_to_primitive_with_hint(
        &mut self,
        value: RegisterValue,
        hint: ToPrimitiveHint,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };

        if self.objects.string_value(handle)?.is_some() {
            return Ok(value);
        }

        let to_primitive =
            self.intern_symbol_property_name(WellKnownSymbol::ToPrimitive.stable_id());
        let exotic =
            self.ordinary_get(handle, to_primitive, value)
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;

        if exotic != RegisterValue::undefined() && exotic != RegisterValue::null() {
            let Some(callable) = exotic.as_object_handle().map(ObjectHandle) else {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive is not callable".into(),
                ));
            };
            if !self.objects.is_callable(callable) {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive is not callable".into(),
                ));
            }

            let hint_value = match hint {
                ToPrimitiveHint::String => self.alloc_string("string"),
                ToPrimitiveHint::Number => self.alloc_string("number"),
            };
            let result = self
                .call_callable(
                    callable,
                    value,
                    &[RegisterValue::from_object_handle(hint_value.0)],
                )
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                    VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
                })?;
            if self.non_string_object_handle(result)?.is_some() {
                return Err(InterpreterError::TypeError(
                    "@@toPrimitive must return a primitive value".into(),
                ));
            }
            return Ok(result);
        }

        self.ordinary_to_primitive(value, hint)
    }

    fn coerce_loose_equality_primitive(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(_handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };
        self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)
    }

    pub(crate) fn js_to_string(
        &mut self,
        value: RegisterValue,
    ) -> Result<Box<str>, InterpreterError> {
        if value == RegisterValue::undefined() {
            return Ok("undefined".into());
        }
        if value == RegisterValue::null() {
            return Ok("null".into());
        }
        if let Some(boolean) = value.as_bool() {
            return Ok(if boolean { "true" } else { "false" }.into());
        }
        if value.is_symbol() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a Symbol value to a string".into(),
            ));
        }
        if let Some(number) = value.as_number() {
            let text = if number.is_nan() {
                "NaN".to_string()
            } else if number.is_infinite() {
                if number.is_sign_positive() {
                    "Infinity".to_string()
                } else {
                    "-Infinity".to_string()
                }
            } else if number == 0.0 {
                "0".to_string()
            } else if number.fract() == 0.0 {
                format!("{number:.0}")
            } else {
                number.to_string()
            };
            return Ok(text.into_boxed_str());
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if let Some(string) = self.objects.string_value(handle)? {
                return Ok(string.to_string().into_boxed_str());
            }
            let primitive = self.js_to_primitive_with_hint(value, ToPrimitiveHint::String)?;
            if primitive != value {
                return self.js_to_string(primitive);
            }
            return Ok("[object Object]".into());
        }

        Ok(String::new().into_boxed_str())
    }

    /// Infallible ToString — returns "" on any error.
    pub fn js_to_string_infallible(&mut self, value: RegisterValue) -> Box<str> {
        self.js_to_string(value).unwrap_or_default()
    }

    /// ES spec 7.1.4 ToNumber — converts a value to its numeric representation.
    pub fn js_to_number(&mut self, value: RegisterValue) -> Result<f64, InterpreterError> {
        if value == RegisterValue::undefined() {
            return Ok(f64::NAN);
        }
        if value == RegisterValue::null() {
            return Ok(0.0);
        }
        if let Some(boolean) = value.as_bool() {
            return Ok(if boolean { 1.0 } else { 0.0 });
        }
        if value.is_symbol() {
            return Err(InterpreterError::TypeError(
                "Cannot convert a Symbol value to a number".into(),
            ));
        }
        if let Some(number) = value.as_number() {
            return Ok(number);
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if let Some(string) = self.objects.string_value(handle)? {
                return Ok(parse_string_to_number(string));
            }
            let primitive = self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)?;
            if primitive != value {
                return self.js_to_number(primitive);
            }
            return Ok(f64::NAN);
        }
        Ok(f64::NAN)
    }

    /// ES spec 7.1.6 ToInt32 — converts a value to a signed 32-bit integer.
    pub fn js_to_int32(&mut self, value: RegisterValue) -> Result<i32, InterpreterError> {
        let n = self.js_to_number(value)?;
        Ok(f64_to_int32(n))
    }

    /// ES spec 7.1.7 ToUint32 — converts a value to an unsigned 32-bit integer.
    pub fn js_to_uint32(&mut self, value: RegisterValue) -> Result<u32, InterpreterError> {
        let n = self.js_to_number(value)?;
        Ok(f64_to_uint32(n))
    }

    /// ES spec 7.1.1 ToPrimitive with hint Number — converts an object to
    /// a primitive value.  Returns the value unchanged for non-objects.
    fn js_to_primitive_number(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        self.js_to_primitive_with_hint(value, ToPrimitiveHint::Number)
    }

    /// ES spec 7.2.13 Abstract Relational Comparison.
    /// Returns `Some(true)` for less-than, `Some(false)` for not less-than,
    /// `None` for undefined (NaN involved).
    fn js_abstract_relational_comparison(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
        left_first: bool,
    ) -> Result<Option<bool>, InterpreterError> {
        // 1-2. ToPrimitive with hint Number.
        let (px, py) = if left_first {
            let px = self.js_to_primitive_number(lhs)?;
            let py = self.js_to_primitive_number(rhs)?;
            (px, py)
        } else {
            let py = self.js_to_primitive_number(rhs)?;
            let px = self.js_to_primitive_number(lhs)?;
            (px, py)
        };

        // 3. If both are strings, compare lexicographically.
        let px_is_string = self.value_is_string(px)?;
        let py_is_string = self.value_is_string(py)?;
        if px_is_string && py_is_string {
            let sx = self.js_to_string(px)?;
            let sy = self.js_to_string(py)?;
            return Ok(Some(sx.as_ref() < sy.as_ref()));
        }

        // 4. Otherwise, coerce both to numbers.
        let nx = self.js_to_number(px)?;
        let ny = self.js_to_number(py)?;
        // NaN comparisons return undefined (None).
        if nx.is_nan() || ny.is_nan() {
            return Ok(None);
        }
        Ok(Some(nx < ny))
    }

    /// ES spec 7.1.2 ToBoolean — runtime-aware truthiness check.
    /// Unlike `RegisterValue::is_truthy()`, this correctly handles heap strings
    /// (empty string "" is falsy).
    pub(crate) fn js_to_boolean(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
        // Fast path: non-object values use the NaN-box check.
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value.is_truthy());
        };
        // Heap strings: empty string is falsy, non-empty is truthy.
        if let Some(s) = self.objects.string_value(handle)? {
            return Ok(!s.is_empty());
        }
        // All other objects are truthy.
        Ok(true)
    }

    /// ES spec §7.3.21 OrdinaryHasInstance — `value instanceof constructor`.
    /// ES2024 §7.3.22 InstanceofOperator(V, target).
    fn js_instance_of(
        &mut self,
        value: RegisterValue,
        constructor: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        // 1. If target is not an Object, throw a TypeError.
        let Some(ctor_handle) = constructor.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Right-hand side of instanceof is not an object".into(),
            ));
        };

        // 2. Let instOfHandler be ? GetMethod(target, @@hasInstance).
        let has_instance_sym =
            self.intern_symbol_property_name(WellKnownSymbol::HasInstance.stable_id());
        let handler = self
            .ordinary_get(ctor_handle, has_instance_sym, constructor)
            .map_err(|error| match error {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;

        // 3. If instOfHandler is not undefined, then
        if handler != RegisterValue::undefined() && handler != RegisterValue::null() {
            let Some(handler_handle) = handler.as_object_handle().map(ObjectHandle) else {
                return Err(InterpreterError::TypeError(
                    "@@hasInstance is not callable".into(),
                ));
            };
            if !self.objects.is_callable(handler_handle) {
                return Err(InterpreterError::TypeError(
                    "@@hasInstance is not callable".into(),
                ));
            }
            // a. Return ! ToBoolean(? Call(instOfHandler, target, « V »)).
            let result = self
                .call_callable(handler_handle, constructor, &[value])
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                    VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                })?;
            return self.js_to_boolean(result);
        }

        // 4. If IsCallable(target) is false, throw a TypeError.
        if !self.objects.is_callable(ctor_handle) {
            return Err(InterpreterError::TypeError(
                "Right-hand side of instanceof is not callable".into(),
            ));
        }

        // 5. Return ? OrdinaryHasInstance(target, V).
        self.ordinary_has_instance(value, ctor_handle)
    }

    /// ES2024 §7.3.21 OrdinaryHasInstance(C, O).
    fn ordinary_has_instance(
        &mut self,
        value: RegisterValue,
        constructor: ObjectHandle,
    ) -> Result<bool, InterpreterError> {
        // 1. If IsCallable(C) is false, return false.
        if !self.objects.is_callable(constructor) {
            return Ok(false);
        }

        // 2. If C has a [[BoundTargetFunction]] internal slot, unwrap.
        let mut effective_ctor = constructor;
        while matches!(
            self.objects.kind(effective_ctor),
            Ok(HeapValueKind::BoundFunction)
        ) {
            let (target, _, _) = self.objects.bound_function_parts(effective_ctor)?;
            effective_ctor = target;
        }

        // 3. If Type(O) is not Object, return false.
        let Some(obj_handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };

        // 4. Let P be ? Get(C, "prototype").
        let proto_prop = self.intern_property_name("prototype");
        let proto_value = self
            .ordinary_get(
                effective_ctor,
                proto_prop,
                RegisterValue::from_object_handle(effective_ctor.0),
            )
            .map_err(|error| match error {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;

        // 5. If Type(P) is not Object, throw a TypeError.
        let Some(proto_handle) = proto_value.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Function has non-object prototype in instanceof check".into(),
            ));
        };

        // 6. Repeat: walk the prototype chain of O.
        let mut current = self.objects.get_prototype(obj_handle)?;
        let mut depth = 0;
        while let Some(p) = current {
            if p == proto_handle {
                return Ok(true);
            }
            depth += 1;
            if depth > 45 {
                break;
            }
            current = self.objects.get_prototype(p)?;
        }
        Ok(false)
    }

    /// ES2024 §13.10.1 The `in` Operator — `HasProperty(object, ToPropertyKey(key))`.
    fn js_has_property(
        &mut self,
        key: RegisterValue,
        object: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        let Some(obj_handle) = self.non_string_object_handle(object)? else {
            return Err(InterpreterError::TypeError(
                "Cannot use 'in' operator to search for property in non-object".into(),
            ));
        };
        let property = self.computed_property_name(key)?;
        self.has_property(obj_handle, property)
            .map_err(InterpreterError::from)
    }

    /// Allocate an error object with the correct prototype chain.
    fn alloc_reference_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics.reference_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Allocate a TypeError object with the correct prototype chain.
    pub fn alloc_type_error(&mut self, message: &str) -> Result<ObjectHandle, InterpreterError> {
        let prototype = self.intrinsics.type_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg_handle = self.objects.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(
            handle,
            msg_prop,
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
        Ok(handle)
    }

    /// Creates a { status: "...", [value_key]: value } object for Promise.allSettled.
    /// ES2024 §27.2.4.2.1–2
    pub fn alloc_settled_result_object(
        &mut self,
        status: &str,
        value_key: &str,
        value: RegisterValue,
    ) -> ObjectHandle {
        let obj = self.alloc_object();
        let status_prop = self.intern_property_name("status");
        let status_str = self.objects.alloc_string(status);
        let _ = self.objects.set_property(
            obj,
            status_prop,
            RegisterValue::from_object_handle(status_str.0),
        );
        let value_prop = self.intern_property_name(value_key);
        let _ = self.objects.set_property(obj, value_prop, value);
        obj
    }

    /// Checks whether a value is a string type (heap string or string wrapper).
    fn value_is_string(&mut self, value: RegisterValue) -> Result<bool, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        if self.objects.string_value(handle)?.is_some() {
            return Ok(true);
        }
        if let Some(inner) = self.string_wrapper_data(handle)?
            && self.objects.string_value(inner)?.is_some()
        {
            return Ok(true);
        }
        Ok(false)
    }

    fn js_add(
        &mut self,
        lhs: RegisterValue,
        rhs: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let lhs = self.js_to_primitive_with_hint(lhs, ToPrimitiveHint::Number)?;
        let rhs = self.js_to_primitive_with_hint(rhs, ToPrimitiveHint::Number)?;
        let lhs_is_string = self.value_is_string(lhs)?;
        let rhs_is_string = self.value_is_string(rhs)?;

        if lhs_is_string || rhs_is_string {
            let mut text = self.js_to_string(lhs)?.into_string();
            text.push_str(&self.js_to_string(rhs)?);
            let value = self.alloc_string(text);
            return Ok(RegisterValue::from_object_handle(value.0));
        }

        if let (Some(lhs_number), Some(rhs_number)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(RegisterValue::from_number(lhs_number + rhs_number));
        }

        lhs.add_i32(rhs).map_err(InterpreterError::InvalidValue)
    }

    fn js_typeof(&mut self, value: RegisterValue) -> Result<RegisterValue, InterpreterError> {
        let kind = if value == RegisterValue::undefined() {
            "undefined"
        } else if value == RegisterValue::null() {
            "object"
        } else if value.as_bool().is_some() {
            "boolean"
        } else if value.is_symbol() {
            "symbol"
        } else if value.as_number().is_some() {
            "number"
        } else if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            match self.objects.kind(handle)? {
                HeapValueKind::String => "string",
                HeapValueKind::HostFunction
                | HeapValueKind::Closure
                | HeapValueKind::BoundFunction
                | HeapValueKind::PromiseCapabilityFunction
                | HeapValueKind::PromiseCombinatorElement
                | HeapValueKind::PromiseFinallyFunction
                | HeapValueKind::PromiseValueThunk => "function",
                HeapValueKind::Object
                | HeapValueKind::Array
                | HeapValueKind::UpvalueCell
                | HeapValueKind::Iterator
                | HeapValueKind::Promise
                | HeapValueKind::Map
                | HeapValueKind::Set
                | HeapValueKind::MapIterator
                | HeapValueKind::SetIterator
                | HeapValueKind::WeakMap
                | HeapValueKind::WeakSet
                | HeapValueKind::Generator => "object",
            }
        } else {
            "undefined"
        };

        let string = self.alloc_string(kind);
        Ok(RegisterValue::from_object_handle(string.0))
    }

    // ─── Generator Support (§27.5) ────────────────────────────────────

    /// Creates a `{ value, done }` iterator result object.
    /// Convenience wrapper around `create_iter_result_object`.
    pub fn create_iter_result(
        &mut self,
        value: RegisterValue,
        done: bool,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        let obj = self.alloc_object();
        let value_prop = self.intern_property_name("value");
        let done_prop = self.intern_property_name("done");
        self.objects
            .set_property(obj, value_prop, value)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        self.objects
            .set_property(obj, done_prop, RegisterValue::from_bool(done))
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        Ok(obj)
    }

    /// Allocates a generator object in SuspendedStart state.
    ///
    /// Called when a generator function is invoked — instead of executing the
    /// body, we create a generator object that will lazily execute on `.next()`.
    pub fn alloc_generator(
        &mut self,
        module: Module,
        function_index: FunctionIndex,
        closure_handle: Option<ObjectHandle>,
        arguments: Vec<RegisterValue>,
    ) -> ObjectHandle {
        let prototype = self.intrinsics.generator_prototype();
        self.objects
            .alloc_generator(Some(prototype), module, function_index, closure_handle, arguments)
    }

    /// Resumes a suspended generator. Called by the native `.next()`, `.return()`,
    /// and `.throw()` methods on `%GeneratorPrototype%`.
    pub(crate) fn resume_generator(
        &mut self,
        generator: ObjectHandle,
        sent_value: RegisterValue,
        resume_kind: crate::intrinsics::GeneratorResumeKind,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Interpreter::resume_generator_impl(self, generator, sent_value, resume_kind)
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
#[derive(Debug, Clone)]
pub struct Interpreter {
    /// Cooperative interrupt flag — when set to `true` by an external thread
    /// (e.g. a watchdog timer), the interpreter stops at the next back-edge.
    /// This mirrors V8's `TerminateExecution` / JSC's `VMTraps::fireTrap()`
    /// pattern: the flag is an `Arc<AtomicBool>` shared with the caller.
    /// Checked only on backward jumps (loop back-edges), so the cost is one
    /// `Relaxed` atomic load per loop iteration (~1-2 CPU cycles, branch
    /// predicted not-taken >99.999% of the time).
    interrupt_flag: Option<Arc<AtomicBool>>,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    /// Creates a new interpreter instance with no interrupt mechanism.
    #[must_use]
    pub fn new() -> Self {
        Self {
            interrupt_flag: None,
        }
    }

    /// Sets a cooperative interrupt flag.  The caller retains a clone of the
    /// `Arc<AtomicBool>` and can set it to `true` from any thread to request
    /// termination.  The interpreter checks the flag on every backward jump
    /// (loop back-edge) — one `Relaxed` atomic load per loop iteration.
    #[must_use]
    pub fn with_interrupt_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.interrupt_flag = Some(flag);
        self
    }

    /// Returns a shareable interrupt flag, creating one if needed.
    pub fn interrupt_flag(&mut self) -> Arc<AtomicBool> {
        if let Some(ref flag) = self.interrupt_flag {
            Arc::clone(flag)
        } else {
            let flag = Arc::new(AtomicBool::new(false));
            self.interrupt_flag = Some(Arc::clone(&flag));
            flag
        }
    }

    /// Checks the interrupt flag and returns an error if it is set.
    #[inline]
    fn check_interrupt(&self) -> Result<(), InterpreterError> {
        if let Some(ref flag) = self.interrupt_flag
            && flag.load(Ordering::Relaxed)
        {
            return Err(InterpreterError::Interrupted);
        }
        Ok(())
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

    /// Executes a module from its entry function with a fresh runtime.
    pub fn execute(&self, module: &Module) -> Result<ExecutionResult, InterpreterError> {
        let mut runtime = RuntimeState::new();
        self.execute_module(module, &mut runtime)
    }

    /// Executes a module using an existing runtime state.
    /// Used by the event loop driver and embedders.
    pub fn execute_module(
        &self,
        module: &Module,
        runtime: &mut RuntimeState,
    ) -> Result<ExecutionResult, InterpreterError> {
        let mut activation = Self::prepare_entry(module);
        let function = module.entry_function();
        if function.frame_layout().receiver_slot().is_some() {
            let global = runtime.intrinsics().global_object();
            activation.set_receiver(function, RegisterValue::from_object_handle(global.0))?;
        }
        self.run_with_runtime(module, &mut activation, runtime)
    }

    /// Calls a JS function (host function or closure) by ObjectHandle.
    ///
    /// This is the entry point for the event loop to invoke timer callbacks,
    /// promise reaction handlers, and microtask callbacks. It handles both
    /// native host functions and compiled closures.
    pub fn call_function(
        runtime: &mut RuntimeState,
        _module: &Module,
        callable: ObjectHandle,
        this_value: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, InterpreterError> {
        let kind = runtime.objects.kind(callable)?;
        match kind {
            HeapValueKind::HostFunction => {
                match Self::invoke_host_function_handle(runtime, callable, this_value, arguments)? {
                    Completion::Return(value) => Ok(value),
                    Completion::Throw(value) => Err(InterpreterError::UncaughtThrow(value)),
                }
            }
            HeapValueKind::Closure => {
                if runtime
                    .objects
                    .closure_flags(callable)
                    .is_ok_and(|flags| flags.is_class_constructor())
                {
                    return Err(InterpreterError::TypeError(
                        "Class constructor cannot be invoked without 'new'".into(),
                    ));
                }
                let module = runtime.objects.closure_module(callable)?;
                let callee_index = runtime.objects.closure_callee(callable)?;
                let callee_function = module
                    .function(callee_index)
                    .ok_or(InterpreterError::InvalidCallTarget)?;
                let register_count = callee_function.frame_layout().register_count();
                // Pass the closure handle so the activation can access upvalues.
                let mut activation = Activation::with_context(
                    callee_index,
                    register_count,
                    FrameMetadata::default(),
                    Some(callable),
                );

                // Set up receiver.
                if callee_function.frame_layout().receiver_slot().is_some() {
                    activation.set_receiver(callee_function, this_value)?;
                }

                // Copy arguments into parameter slots.
                let param_count = callee_function.frame_layout().parameter_count();
                for (i, &arg) in arguments.iter().take(param_count as usize).enumerate() {
                    let abs = callee_function
                        .frame_layout()
                        .resolve_user_visible(i as u16)
                        .ok_or(InterpreterError::RegisterOutOfBounds)?;
                    activation.set_register(abs, arg)?;
                }

                // ES2024 §10.4.4: Preserve overflow arguments for CreateArguments.
                if arguments.len() > param_count as usize {
                    activation.overflow_args = arguments[param_count as usize..].to_vec();
                }
                // Store actual argument count in metadata.
                activation.metadata =
                    FrameMetadata::new(arguments.len() as RegisterIndex, FrameFlags::default());

                let interpreter = Interpreter::new();
                let result = interpreter.run_with_runtime(&module, &mut activation, runtime)?;
                Ok(result.return_value())
            }
            HeapValueKind::PromiseCapabilityFunction => {
                let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
                Self::invoke_promise_capability_function(runtime, callable, value)?;
                Ok(RegisterValue::undefined())
            }
            HeapValueKind::PromiseCombinatorElement => {
                let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
                Self::invoke_promise_combinator_element(runtime, callable, value)
            }
            HeapValueKind::PromiseFinallyFunction => {
                let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
                Self::invoke_promise_finally_function(runtime, callable, value)
            }
            HeapValueKind::PromiseValueThunk => {
                // §27.2.5.3.1 step 8 / §27.2.5.3.2 step 8
                let (thunk_value, thunk_kind) = runtime
                    .objects
                    .promise_value_thunk_info(callable)
                    .ok_or(InterpreterError::InvalidHeapValueKind)?;
                match thunk_kind {
                    crate::promise::PromiseFinallyKind::ThenFinally => Ok(thunk_value),
                    crate::promise::PromiseFinallyKind::CatchFinally => {
                        Err(InterpreterError::UncaughtThrow(thunk_value))
                    }
                }
            }
            _ => Err(InterpreterError::TypeError(
                format!("{kind:?} is not a function").into(),
            )),
        }
    }

    /// Invokes a PromiseCapabilityFunction (resolve or reject) with a value.
    /// ES2024 §27.2.1.3.1 Promise Reject Functions / §27.2.1.3.2 Promise Resolve Functions
    fn invoke_promise_capability_function(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(), InterpreterError> {
        let (promise_handle, kind) = runtime
            .objects
            .promise_capability_function_info(callable)
            .ok_or_else(|| {
                InterpreterError::TypeError("not a promise capability function".into())
            })?;

        let promise = runtime
            .objects
            .get_promise_mut(promise_handle)
            .ok_or_else(|| {
                InterpreterError::TypeError("promise capability target is not a promise".into())
            })?;

        // §27.2.1.3: If alreadyResolved is true, return undefined.
        // We use is_pending() — once settled, further calls are no-ops.
        if !promise.is_pending() {
            return Ok(());
        }

        let jobs = match kind {
            crate::promise::ReactionKind::Fulfill => {
                // §27.2.1.3.2 step 8: If value is the same promise, reject with TypeError.
                if let Some(h) = value.as_object_handle() {
                    if h == promise_handle.0 {
                        let err_handle = runtime
                            .alloc_type_error("A promise cannot be resolved with itself")
                            .map_err(|_| InterpreterError::InvalidHeapValueKind)?;
                        let promise = runtime.objects.get_promise_mut(promise_handle).unwrap();
                        promise.reject(RegisterValue::from_object_handle(err_handle.0))
                    } else {
                        // §27.2.1.3.2 step 9-11: If value is a thenable (another promise),
                        // we need to chain. For now, check if value is a promise and chain.
                        if runtime.objects.get_promise(ObjectHandle(h)).is_some() {
                            // Value is a promise — register then reactions to forward settlement.
                            Self::chain_promise_resolution(runtime, promise_handle, ObjectHandle(h));
                            return Ok(());
                        }
                        let promise = runtime.objects.get_promise_mut(promise_handle).unwrap();
                        promise.fulfill(value)
                    }
                } else {
                    promise.fulfill(value)
                }
            }
            crate::promise::ReactionKind::Reject => promise.reject(value),
        };

        if let Some(jobs) = jobs {
            for job in jobs {
                runtime.microtasks_mut().enqueue_promise_job(job);
            }
        }

        Ok(())
    }

    /// Chains a thenable promise resolution: when `thenable` settles, forward to `promise`.
    /// ES2024 §27.2.1.3.2 step 12 — HostEnqueuePromiseJob(PromiseResolveThenableJob)
    fn chain_promise_resolution(
        runtime: &mut RuntimeState,
        promise: ObjectHandle,
        thenable: ObjectHandle,
    ) {
        // Get or create resolve/reject for the target promise.
        let resolve = runtime.objects.alloc_promise_capability_function(
            promise,
            crate::promise::ReactionKind::Fulfill,
        );
        let reject = runtime.objects.alloc_promise_capability_function(
            promise,
            crate::promise::ReactionKind::Reject,
        );

        let capability = crate::promise::PromiseCapability {
            promise,
            resolve,
            reject,
        };

        // Register reactions on the thenable.
        let thenable_promise = runtime
            .objects
            .get_promise_mut(thenable)
            .expect("thenable verified as promise");

        if let Some(immediate_job) = thenable_promise.then(
            Some(resolve),
            Some(reject),
            capability,
        ) {
            runtime.microtasks_mut().enqueue_promise_job(immediate_job);
        }
    }

    /// Invokes a PromiseCombinatorElement (per-element resolve/reject for all/allSettled/any).
    /// ES2024 §27.2.4.1.1, §27.2.4.2.1–2, §27.2.4.3.1
    fn invoke_promise_combinator_element(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        use crate::promise::PromiseCombinatorKind;

        // Extract all fields from the combinator element.
        let (combinator_kind, index, result_array, remaining_counter, result_cap, already_called) =
            runtime
                .objects
                .promise_combinator_element_info(callable)
                .ok_or_else(|| {
                    InterpreterError::TypeError("not a promise combinator element".into())
                })?;

        // §27.2.4.1.1 step 1: If alreadyCalled is true, return undefined.
        if already_called {
            return Ok(RegisterValue::undefined());
        }

        // Set alreadyCalled to true.
        runtime.objects.set_combinator_element_called(callable);

        match combinator_kind {
            PromiseCombinatorKind::AllResolve => {
                // §27.2.4.1.1: Store value at result_array[index].
                let _ = runtime
                    .objects
                    .set_index(result_array, index as usize, value);

                // Decrement remaining counter.
                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    // All elements resolved — fulfill the result promise with the array.
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.resolve,
                        RegisterValue::from_object_handle(result_array.0),
                    )?;
                }
            }
            PromiseCombinatorKind::AllSettledResolve => {
                // §27.2.4.2.1: Create { status: "fulfilled", value: value }.
                let obj = runtime.alloc_settled_result_object("fulfilled", "value", value);
                let _ = runtime.objects.set_index(
                    result_array,
                    index as usize,
                    RegisterValue::from_object_handle(obj.0),
                );

                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.resolve,
                        RegisterValue::from_object_handle(result_array.0),
                    )?;
                }
            }
            PromiseCombinatorKind::AllSettledReject => {
                // §27.2.4.2.2: Create { status: "rejected", reason: value }.
                let obj = runtime.alloc_settled_result_object("rejected", "reason", value);
                let _ = runtime.objects.set_index(
                    result_array,
                    index as usize,
                    RegisterValue::from_object_handle(obj.0),
                );

                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.resolve,
                        RegisterValue::from_object_handle(result_array.0),
                    )?;
                }
            }
            PromiseCombinatorKind::AnyReject => {
                // §27.2.4.3.1: Store error at result_array[index] (errors array).
                let _ = runtime
                    .objects
                    .set_index(result_array, index as usize, value);

                if Self::decrement_combinator_counter(runtime, remaining_counter) {
                    // All elements rejected — reject with AggregateError.
                    let err = runtime
                        .alloc_type_error("All promises were rejected")
                        .map_err(|_| InterpreterError::InvalidHeapValueKind)?;
                    // Attach errors array as property.
                    let errors_prop = runtime.intern_property_name("errors");
                    let _ = runtime.objects.set_property(
                        err,
                        errors_prop,
                        RegisterValue::from_object_handle(result_array.0),
                    );
                    Self::invoke_promise_capability_function(
                        runtime,
                        result_cap.reject,
                        RegisterValue::from_object_handle(err.0),
                    )?;
                }
            }
        }

        Ok(RegisterValue::undefined())
    }

    /// Decrements the counter in remaining_counter[0] and returns true if it reached 0.
    fn decrement_combinator_counter(
        runtime: &mut RuntimeState,
        counter_handle: ObjectHandle,
    ) -> bool {
        let Ok(elements) = runtime.objects.array_elements(counter_handle) else {
            return false;
        };
        let count = elements
            .first()
            .and_then(|v| v.as_i32())
            .unwrap_or(0);
        let new_count = count - 1;
        let _ = runtime
            .objects
            .set_index(counter_handle, 0, RegisterValue::from_i32(new_count));
        new_count == 0
    }

    /// Invokes a PromiseFinallyFunction (ThenFinally/CatchFinally wrapper).
    /// ES2024 §27.2.5.3.1–2
    fn invoke_promise_finally_function(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        use crate::promise::PromiseFinallyKind;

        let (on_finally, _constructor, kind) = runtime
            .objects
            .promise_finally_function_info(callable)
            .ok_or_else(|| {
                InterpreterError::TypeError("not a promise finally function".into())
            })?;

        // Call onFinally() with no arguments.
        let finally_result = runtime.call_host_function(
            Some(on_finally),
            RegisterValue::undefined(),
            &[],
        );

        match kind {
            PromiseFinallyKind::ThenFinally => {
                // §27.2.5.3.1: If onFinally() throws, propagate.
                // If it returns normally, return the original value.
                match finally_result {
                    Ok(_) => Ok(value),
                    Err(VmNativeCallError::Thrown(thrown)) => {
                        Err(InterpreterError::UncaughtThrow(thrown))
                    }
                    Err(VmNativeCallError::Internal(msg)) => {
                        Err(InterpreterError::NativeCall(msg))
                    }
                }
            }
            PromiseFinallyKind::CatchFinally => {
                // §27.2.5.3.2: If onFinally() throws, propagate that throw.
                // If it returns normally, re-throw the original reason.
                match finally_result {
                    Ok(_) => Err(InterpreterError::UncaughtThrow(value)),
                    Err(VmNativeCallError::Thrown(thrown)) => {
                        Err(InterpreterError::UncaughtThrow(thrown))
                    }
                    Err(VmNativeCallError::Internal(msg)) => {
                        Err(InterpreterError::NativeCall(msg))
                    }
                }
            }
        }
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
            activation.begin_step();
            match self.step(
                function,
                module,
                &mut activation,
                &mut runtime,
                &mut frame_runtime,
            )? {
                StepOutcome::Continue => {
                    activation.sync_written_open_upvalues(&mut runtime)?;
                    activation.refresh_open_upvalues_from_cells(&runtime)?;
                }
                StepOutcome::Return(_) => {
                    return Ok(frame_runtime.property_feedback);
                }
                StepOutcome::Throw(value) => {
                    return Err(InterpreterError::UncaughtThrow(value));
                }
                StepOutcome::Suspend { .. } => {
                    // Suspension not supported in feedback-collection mode.
                    return Err(InterpreterError::TypeError(
                        "await is not supported in this execution mode".into(),
                    ));
                }
                StepOutcome::GeneratorYield { .. } => {
                    // Yield not supported in feedback-collection mode.
                    return Ok(frame_runtime.property_feedback);
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
        let previous_module = runtime.enter_module(module);
        let function = module
            .function(activation.function_index())
            .expect("activation function index must be valid");

        let mut frame_runtime = FrameRuntimeState::new(function);

        loop {
            activation.begin_step();
            let outcome = match self.step(function, module, activation, runtime, &mut frame_runtime)
            {
                Ok(outcome) => outcome,
                Err(InterpreterError::UncaughtThrow(value)) => StepOutcome::Throw(value),
                Err(InterpreterError::TypeError(message)) => {
                    let error = runtime.alloc_type_error(&message)?;
                    StepOutcome::Throw(RegisterValue::from_object_handle(error.0))
                }
                Err(error) => {
                    runtime.restore_module(previous_module);
                    return Err(error);
                }
            };

            match outcome {
                StepOutcome::Continue => {
                    activation.sync_written_open_upvalues(runtime)?;
                    activation.refresh_open_upvalues_from_cells(runtime)?;
                }
                StepOutcome::Return(return_value) => {
                    runtime.restore_module(previous_module);
                    return Ok(Completion::Return(return_value));
                }
                StepOutcome::Throw(value) => {
                    if self.transfer_exception(function, activation, value) {
                        continue;
                    }
                    runtime.restore_module(previous_module);
                    return Ok(Completion::Throw(value));
                }
                StepOutcome::Suspend { .. } => {
                    // TODO: Capture SuspendedFrame and return to event loop.
                    // For now, async functions are not fully wired — treat as
                    // returning undefined (the result_promise will be settled
                    // when the event loop integrates suspend/resume).
                    runtime.restore_module(previous_module);
                    return Ok(Completion::Return(RegisterValue::undefined()));
                }
                StepOutcome::GeneratorYield { yielded_value, .. } => {
                    // GeneratorYield inside a non-generator run loop — treat
                    // as a return (shouldn't normally happen outside resume_generator).
                    runtime.restore_module(previous_module);
                    return Ok(Completion::Return(yielded_value));
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
            Opcode::LoadNaN => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(f64::NAN),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadF64 => {
                let float_id = FloatId(instruction.b());
                let value = function
                    .float_constants()
                    .get(float_id)
                    .ok_or(InterpreterError::InvalidConstant)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(value),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewObject => {
                let handle = runtime.alloc_object();
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
                let handle = runtime.alloc_string(string);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::NewArray => {
                let handle = runtime.alloc_array();
                let len = instruction.b() as usize;
                if len > 0 {
                    runtime.objects_mut().set_array_length(handle, len)?;
                }
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
                let mut upvalues = Vec::with_capacity(usize::from(template.capture_count()));

                for capture in template.captures() {
                    let upvalue = match capture {
                        CaptureDescriptor::Register(register) => activation
                            .capture_bytecode_register_upvalue(function, runtime, *register)?,
                        CaptureDescriptor::Upvalue(upvalue) => {
                            Self::resolve_upvalue_cell(activation, runtime, *upvalue)?
                        }
                    };
                    upvalues.push(upvalue);
                }

                let handle = runtime.alloc_closure(template.callee(), upvalues, template.flags());
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // -----------------------------------------------------------------
            // ES2024 §10.4.4 CreateArguments — creates the arguments exotic object.
            //
            // Collects formal parameter values from the activation register file
            // and overflow arguments from `activation.overflow_args`, then builds
            // an arguments object with:
            //   - Indexed element access (§10.4.4.1 [[GetOwnProperty]])
            //   - `length` property = actual argument count (§10.4.4.6 step 7)
            //   - `callee` property = current closure (sloppy mode, §10.4.4.7 step 13)
            //   - Prototype = %Object.prototype% (NOT Array.prototype)
            // -----------------------------------------------------------------
            Opcode::CreateArguments => {
                let actual_argc = activation.metadata.argument_count();
                let param_count = function.frame_layout().parameter_count();
                let param_range = function.frame_layout().parameter_range();

                // Collect all actual arguments: formal params from registers + overflow.
                let mut all_args = Vec::with_capacity(usize::from(actual_argc));
                let copy_from_regs = actual_argc.min(param_count);
                for i in 0..copy_from_regs {
                    let value = activation
                        .read_bytecode_register(function, param_range.start().saturating_add(i))?;
                    all_args.push(value);
                }
                for overflow_val in &activation.overflow_args {
                    all_args.push(*overflow_val);
                }

                // Create arguments exotic object backed by an Array with Object.prototype.
                let obj_proto = runtime.intrinsics().object_prototype();
                let args_obj = runtime.alloc_array_with_elements(&all_args);
                // §10.4.4.6 step 4: Set prototype to %Object.prototype% (not Array.prototype).
                runtime
                    .objects_mut()
                    .set_prototype(args_obj, Some(obj_proto))
                    .ok();

                // §10.4.4.6 step 7: Install `length` as own data property {W:true, E:false, C:true}.
                let length_key = runtime.intern_property_name("length");
                runtime
                    .objects_mut()
                    .define_own_property(
                        args_obj,
                        length_key,
                        PropertyValue::data_with_attrs(
                            RegisterValue::from_i32(i32::from(actual_argc)),
                            PropertyAttributes::builtin_method(),
                        ),
                    )
                    .ok();

                // §10.4.4.7 step 13: Install `callee` (sloppy mode only).
                // For now, install if closure is available.
                if let Some(closure) = activation.closure_handle() {
                    let callee_key = runtime.intern_property_name("callee");
                    runtime
                        .objects_mut()
                        .define_own_property(
                            args_obj,
                            callee_key,
                            PropertyValue::data_with_attrs(
                                RegisterValue::from_object_handle(closure.0),
                                PropertyAttributes::builtin_method(),
                            ),
                        )
                        .ok();
                }

                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(args_obj.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CreateRestParameters => {
                let rest_array = runtime.alloc_array_with_elements(&activation.overflow_args);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(rest_array.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CreateEnumerableOwnKeys => {
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                let keys =
                    runtime
                        .enumerable_own_property_keys(handle)
                        .map_err(|error| match error {
                            VmNativeCallError::Thrown(_) => {
                                InterpreterError::TypeError("enumerable own keys threw".into())
                            }
                            VmNativeCallError::Internal(message) => {
                                InterpreterError::NativeCall(message)
                            }
                        })?;
                let key_names = keys
                    .into_iter()
                    .filter_map(|key| runtime.property_names().get(key))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let key_values = key_names
                    .into_iter()
                    .map(|name| RegisterValue::from_object_handle(runtime.alloc_string(name).0))
                    .collect::<Vec<_>>();
                let keys_array = runtime.alloc_array_with_elements(&key_values);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(keys_array.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::LoadHole => {
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::hole(),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::AssertNotHole => {
                let value = activation.read_bytecode_register(function, instruction.a())?;
                if value.is_hole() {
                    let error =
                        runtime.alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::DefineNamedGetter
            | Opcode::DefineNamedSetter
            | Opcode::DefineComputedGetter
            | Opcode::DefineComputedSetter => {
                let object = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(object)?;
                let (property, accessor_register) = match instruction.opcode() {
                    Opcode::DefineNamedGetter | Opcode::DefineNamedSetter => (
                        Self::resolve_property_name(function, runtime, instruction.c())?,
                        instruction.b(),
                    ),
                    Opcode::DefineComputedGetter | Opcode::DefineComputedSetter => {
                        let key = activation.read_bytecode_register(function, instruction.b())?;
                        (runtime.computed_property_name(key)?, instruction.c())
                    }
                    _ => unreachable!(),
                };
                let accessor = activation
                    .read_bytecode_register(function, accessor_register)?
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let desc = match instruction.opcode() {
                    Opcode::DefineNamedGetter => crate::object::PropertyDescriptor::accessor(
                        Some(Some(accessor)),
                        None,
                        Some(true),
                        Some(true),
                    ),
                    Opcode::DefineNamedSetter => crate::object::PropertyDescriptor::accessor(
                        None,
                        Some(Some(accessor)),
                        Some(true),
                        Some(true),
                    ),
                    Opcode::DefineComputedGetter => crate::object::PropertyDescriptor::accessor(
                        Some(Some(accessor)),
                        None,
                        Some(true),
                        Some(true),
                    ),
                    Opcode::DefineComputedSetter => crate::object::PropertyDescriptor::accessor(
                        None,
                        Some(Some(accessor)),
                        Some(true),
                        Some(true),
                    ),
                    _ => unreachable!(),
                };
                let defined = runtime
                    .objects
                    .define_own_property_from_descriptor(handle, property, desc)?;
                if !defined {
                    return Err(InterpreterError::TypeError(
                        "object literal accessor define failed".into(),
                    ));
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::CopyDataProperties | Opcode::CopyDataPropertiesExcept => {
                let target = activation.read_bytecode_register(function, instruction.a())?;
                let target_handle = runtime.property_base_object_handle(target)?;
                let source = activation.read_bytecode_register(function, instruction.b())?;
                let excluded_keys = if instruction.opcode() == Opcode::CopyDataPropertiesExcept {
                    Some(activation.read_bytecode_register(function, instruction.c())?)
                } else {
                    None
                };
                match crate::property_copy::copy_data_properties(
                    runtime,
                    target_handle,
                    source,
                    excluded_keys,
                ) {
                    Ok(()) => {
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
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
                if function.is_derived_constructor()
                    && activation.metadata().flags().is_construct()
                    && receiver == RegisterValue::undefined()
                {
                    let error = runtime.alloc_reference_error(
                        "Must call super constructor in derived class before accessing 'this'",
                    )?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }
                activation.write_bytecode_register(function, instruction.a(), receiver)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::TypeOf => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let type_of = runtime.js_typeof(value)?;
                activation.write_bytecode_register(function, instruction.a(), type_of)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Not => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let truthy = runtime.js_to_boolean(value)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(!truthy),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::ToNumber => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let number = runtime.js_to_number(value)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(number),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::ToString => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let text = runtime.js_to_string(value)?;
                let string = runtime.alloc_string(text);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(string.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Add => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let value = runtime.js_add(lhs, rhs)?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Sub => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num - rhs_num),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Mul => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num * rhs_num),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Div => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num / rhs_num),
                )?;
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
            Opcode::LooseEq => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(runtime.js_loose_eq(lhs, rhs)?),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // ES spec 7.2.13 Abstract Relational Comparison.
            // Lt(a, b, c): a = (b < c)
            Opcode::Lt => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                // AbstractRelationalComparison(x, y, LeftFirst=true) → true means x < y
                let result = runtime
                    .js_abstract_relational_comparison(lhs, rhs, true)?
                    .unwrap_or(false);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Gt(a, b, c): a = (b > c) ≡ AbstractRelationalComparison(c, b, LeftFirst=false)
            Opcode::Gt => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let result = runtime
                    .js_abstract_relational_comparison(rhs, lhs, false)?
                    .unwrap_or(false);
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Gte(a, b, c): a = (b >= c) ≡ !(c < b ... wait, no)
            // ES spec: x >= y ≡ NOT AbstractRelationalComparison(x, y) where undefined → false
            Opcode::Gte => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                // x >= y: if AbstractRelationalComparison(x, y) is undefined or true → false
                let less = runtime.js_abstract_relational_comparison(lhs, rhs, true)?;
                let result = match less {
                    None => false,       // undefined (NaN) → false
                    Some(true) => false, // x < y → not >=
                    Some(false) => true, // x >= y
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Lte(a, b, c): a = (b <= c) ≡ NOT AbstractRelationalComparison(c, b, LeftFirst=false)
            Opcode::Lte => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let greater = runtime.js_abstract_relational_comparison(rhs, lhs, false)?;
                let result = match greater {
                    None => false,
                    Some(true) => false,
                    Some(false) => true,
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // Mod uses ToNumber coercion.
            Opcode::Mod => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_num = runtime.js_to_number(lhs)?;
                let rhs_num = runtime.js_to_number(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number(lhs_num % rhs_num),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::BitAnd => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_i32 = runtime.js_to_int32(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 & rhs_i32) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::BitOr => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_i32 = runtime.js_to_int32(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 | rhs_i32) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::BitXor => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_i32 = runtime.js_to_int32(rhs)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 ^ rhs_i32) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Shl => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_u32 = runtime.js_to_uint32(rhs)?;
                let shift = rhs_u32 & 0x1F;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 << shift) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::Shr => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_i32 = runtime.js_to_int32(lhs)?;
                let rhs_u32 = runtime.js_to_uint32(rhs)?;
                let shift = rhs_u32 & 0x1F;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_i32 >> shift) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::UShr => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let lhs_u32 = runtime.js_to_uint32(lhs)?;
                let rhs_u32 = runtime.js_to_uint32(rhs)?;
                let shift = rhs_u32 & 0x1F;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_number((lhs_u32 >> shift) as f64),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetProperty => {
                let pc = activation.pc();
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
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

                let supports_inline_property_cache = !matches!(
                    runtime.objects.kind(handle)?,
                    HeapValueKind::Array | HeapValueKind::String
                );
                let value = if supports_inline_property_cache {
                    if let Some(cache) = frame_runtime.property_cache(function, pc) {
                        match runtime.objects.get_cached(handle, property, cache)? {
                            Some(PropertyValue::Data { value, .. }) => value,
                            Some(PropertyValue::Accessor { getter, .. }) => {
                                runtime.call_callable_for_accessor(getter, base, &[])?
                            }
                            None => Self::generic_get_property(
                                function,
                                runtime,
                                frame_runtime,
                                pc,
                                handle,
                                base,
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
                            base,
                            property,
                        )?
                    }
                } else {
                    Self::generic_get_property(
                        function,
                        runtime,
                        frame_runtime,
                        pc,
                        handle,
                        base,
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
                let base = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_set_target_handle(base)?;
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let primitive_base = runtime.is_primitive_property_base(base)?;

                if primitive_base {
                    let handled =
                        Self::primitive_set_property(runtime, handle, base, property, value)?;
                    if !handled && function.is_strict() {
                        let error = runtime
                            .alloc_type_error("Cannot assign to property of primitive value")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                let supports_inline_property_cache = !matches!(
                    runtime.objects.kind(handle)?,
                    HeapValueKind::Array | HeapValueKind::String
                );
                let handled = if supports_inline_property_cache {
                    if let Some(cache) = frame_runtime.property_cache(function, pc) {
                        match runtime.objects.get_cached(handle, property, cache)? {
                            Some(PropertyValue::Data { .. }) => {
                                runtime.objects.set_cached(handle, property, value, cache)?
                            }
                            Some(PropertyValue::Accessor { setter, .. }) => {
                                let _ =
                                    runtime.call_callable_for_accessor(setter, base, &[value])?;
                                true
                            }
                            None => Self::generic_set_property(
                                function,
                                runtime,
                                frame_runtime,
                                pc,
                                handle,
                                base,
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
                            base,
                            property,
                            value,
                        )?
                    }
                } else {
                    Self::generic_set_property(
                        function,
                        runtime,
                        frame_runtime,
                        pc,
                        handle,
                        base,
                        property,
                        value,
                    )?
                };

                if !handled {
                    let cache = runtime.set_named_property(handle, property, value)?;
                    if supports_inline_property_cache {
                        frame_runtime.update_property_cache(function, pc, cache);
                    }
                }

                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::DeleteProperty => {
                let property = Self::resolve_property_name(function, runtime, instruction.c())?;
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                let deleted = runtime.delete_named_property(handle, property)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(deleted),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::DeleteComputed => {
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                let key_value = activation.read_bytecode_register(function, instruction.c())?;
                let property = runtime.computed_property_name(key_value)?;
                let deleted = runtime.delete_named_property(handle, property)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(deleted),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetIndex => {
                let pc = activation.pc();
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = runtime.property_base_object_handle(base)?;
                let key = activation.read_bytecode_register(function, instruction.c())?;
                let property = runtime.computed_property_name(key)?;
                let value = Self::generic_get_property(
                    function,
                    runtime,
                    frame_runtime,
                    pc,
                    handle,
                    base,
                    property,
                )?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetIndex => {
                let pc = activation.pc();
                let base = activation.read_bytecode_register(function, instruction.a())?;
                let key = activation.read_bytecode_register(function, instruction.b())?;
                let value = activation.read_bytecode_register(function, instruction.c())?;
                let property = runtime.computed_property_name(key)?;
                let handle = runtime.property_set_target_handle(base)?;
                let primitive_base = runtime.is_primitive_property_base(base)?;

                if primitive_base {
                    let handled =
                        Self::primitive_set_property(runtime, handle, base, property, value)?;
                    if !handled && function.is_strict() {
                        let error = runtime
                            .alloc_type_error("Cannot assign to property of primitive value")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                match runtime.objects.kind(handle)? {
                    HeapValueKind::Array => {
                        let handled = Self::generic_set_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            base,
                            property,
                            value,
                        )?;

                        if !handled {
                            runtime.set_named_property(handle, property, value)?;
                        }
                    }
                    HeapValueKind::String => {}
                    _ => {
                        let handled = Self::generic_set_property(
                            function,
                            runtime,
                            frame_runtime,
                            pc,
                            handle,
                            base,
                            property,
                            value,
                        )?;

                        if !handled {
                            let cache = runtime.set_named_property(handle, property, value)?;
                            frame_runtime.update_property_cache(function, pc, cache);
                        }
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetIterator => {
                let base = activation.read_bytecode_register(function, instruction.b())?;
                let handle = base
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::TypeError("Value is not iterable".into()))?;

                // Fast path: internal iterators for Array and String.
                let iterator = match runtime.objects.alloc_iterator(handle) {
                    Ok(iterator) => iterator,
                    Err(ObjectError::InvalidKind) => {
                        // Slow path: look up Symbol.iterator method.
                        let sym_iterator = runtime.intern_symbol_property_name(
                            super::WellKnownSymbol::Iterator.stable_id(),
                        );
                        let method = runtime.ordinary_get(handle, sym_iterator, base).map_err(
                            |e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            },
                        )?;
                        let callable = method
                            .as_object_handle()
                            .map(ObjectHandle)
                            .filter(|h| runtime.objects.is_callable(*h))
                            .ok_or_else(|| {
                                InterpreterError::TypeError("Value is not iterable".into())
                            })?;
                        let iter_obj =
                            runtime
                                .call_callable(callable, base, &[])
                                .map_err(|e| match e {
                                    VmNativeCallError::Thrown(v) => {
                                        InterpreterError::UncaughtThrow(v)
                                    }
                                    VmNativeCallError::Internal(m) => {
                                        InterpreterError::NativeCall(m)
                                    }
                                })?;
                        iter_obj.as_object_handle().map(ObjectHandle).ok_or(
                            InterpreterError::TypeError(
                                "Symbol.iterator must return an object".into(),
                            ),
                        )?
                    }
                    Err(error) => return Err(error.into()),
                };
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
                // Fast path: internal iterators.
                let step = match runtime.iterator_next(iterator) {
                    Ok(step) => step,
                    Err(InterpreterError::InvalidHeapValueKind) => {
                        // Slow path: protocol-based iterator — call .next().
                        let next_prop = runtime.intern_property_name("next");
                        let iter_val = RegisterValue::from_object_handle(iterator.0);
                        let next_fn = runtime
                            .ordinary_get(iterator, next_prop, iter_val)
                            .map_err(|e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            })?;
                        let callable = next_fn
                            .as_object_handle()
                            .map(ObjectHandle)
                            .filter(|h| runtime.objects.is_callable(*h))
                            .ok_or_else(|| {
                                InterpreterError::TypeError(
                                    "Iterator .next is not a function".into(),
                                )
                            })?;
                        let result_obj = runtime.call_callable(callable, iter_val, &[]).map_err(
                            |e| match e {
                                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                            },
                        )?;
                        let result_handle = result_obj.as_object_handle().map(ObjectHandle).ok_or(
                            InterpreterError::TypeError(
                                "Iterator .next() must return an object".into(),
                            ),
                        )?;
                        let done_prop = runtime.intern_property_name("done");
                        let done_val = runtime
                            .ordinary_get(result_handle, done_prop, result_obj)
                            .unwrap_or_else(|_| RegisterValue::from_bool(false));
                        let done = runtime.js_to_boolean(done_val).unwrap_or(false);
                        if done {
                            crate::object::IteratorStep::done()
                        } else {
                            let value_prop = runtime.intern_property_name("value");
                            let value = runtime
                                .ordinary_get(result_handle, value_prop, result_obj)
                                .unwrap_or_else(|_| RegisterValue::undefined());
                            crate::object::IteratorStep::yield_value(value)
                        }
                    }
                    Err(e) => return Err(e),
                };
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
            // V8-style LdaGlobal: load a global variable by name from the
            // global object (receiver r0).  Throws if not found.
            Opcode::GetGlobal => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let global_handle = runtime.intrinsics().global_object();
                let value = runtime.objects.get_property(global_handle, property)?;
                match value {
                    Some(lookup) => {
                        let val = match lookup.value() {
                            PropertyValue::Data { value: v, .. } => v,
                            PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                        };
                        activation.write_bytecode_register(function, instruction.a(), val)?;
                    }
                    None => {
                        // Property not found → throw (ReferenceError semantics).
                        let name = runtime.property_names().get(property).unwrap_or("?");
                        let msg = format!("{name} is not defined");
                        let error_obj = runtime.alloc_reference_error(&msg)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error_obj.0,
                        )));
                    }
                }
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetGlobal => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let value = activation.read_bytecode_register(function, instruction.a())?;
                let global_handle = runtime.intrinsics().global_object();
                runtime
                    .objects
                    .set_property(global_handle, property, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // typeof on a global variable — returns "undefined" for unresolvable.
            // ES2024 §13.5.1: typeof on an unresolvable Reference returns "undefined".
            Opcode::TypeOfGlobal => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let global_handle = runtime.intrinsics().global_object();
                let value = runtime.objects.get_property(global_handle, property)?;
                let val = match value {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Data { value: v, .. } => v,
                        PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                    },
                    None => RegisterValue::undefined(),
                };
                let type_val = runtime.js_typeof(val)?;
                activation.write_bytecode_register(function, instruction.a(), type_val)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetPropertyIterator => {
                let object_val = activation.read_bytecode_register(function, instruction.b())?;
                // ES spec 13.7.5.15: for-in on null/undefined produces no iterations.
                // Primitives (number, bool) have no enumerable own properties.
                let iter_handle = if object_val == RegisterValue::null()
                    || object_val == RegisterValue::undefined()
                {
                    runtime.alloc_empty_property_iterator()?
                } else if let Some(handle) = object_val.as_object_handle().map(ObjectHandle) {
                    runtime.alloc_property_iterator(handle)?
                } else {
                    runtime.alloc_empty_property_iterator()?
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_object_handle(iter_handle.0),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::PropertyIteratorNext => {
                let iter_val = activation.read_bytecode_register(function, instruction.c())?;
                let iter_handle = iter_val
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                let step = runtime.objects.property_iterator_next(iter_handle)?;
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(step.is_done()),
                )?;
                activation.write_bytecode_register(function, instruction.b(), step.value())?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // ES spec §7.3.21 OrdinaryHasInstance — `lhs instanceof rhs`.
            Opcode::InstanceOf => {
                let lhs = activation.read_bytecode_register(function, instruction.b())?;
                let rhs = activation.read_bytecode_register(function, instruction.c())?;
                let result = match runtime.js_instance_of(lhs, rhs) {
                    Ok(result) => result,
                    Err(InterpreterError::TypeError(message)) => {
                        let error = runtime.alloc_type_error(&message)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    Err(error) => return Err(error),
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // `in` operator — check if property exists on object.
            Opcode::HasProperty => {
                let key = activation.read_bytecode_register(function, instruction.b())?;
                let object = activation.read_bytecode_register(function, instruction.c())?;
                let result = match runtime.js_has_property(key, object) {
                    Ok(result) => result,
                    Err(InterpreterError::TypeError(message)) => {
                        let error = runtime.alloc_type_error(&message)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    Err(error) => return Err(error),
                };
                activation.write_bytecode_register(
                    function,
                    instruction.a(),
                    RegisterValue::from_bool(result),
                )?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::GetUpvalue => {
                let upvalue =
                    Self::resolve_upvalue_cell(activation, runtime, UpvalueId(instruction.b()))?;
                let value = runtime.objects.get_upvalue(upvalue)?;
                if value.is_hole() {
                    let error =
                        runtime.alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }
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
                let callee_value =
                    activation.read_bytecode_register(caller_function, instruction.b())?;
                let Some(callee) = callee_value.as_object_handle().map(ObjectHandle) else {
                    let error = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };

                // ES2024 §10.4.1.1 [[Call]] — resolve bound function before dispatch.
                let arguments = Self::read_call_arguments(
                    caller_function,
                    activation,
                    instruction.c(),
                    call.argument_count(),
                )?;
                if call.flags().is_construct() {
                    if !runtime.is_constructible(callee) {
                        let error = runtime.alloc_type_error("Value is not a constructor")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error.0,
                        )));
                    }
                    match runtime.construct_callable(callee, &arguments, callee) {
                        Ok(value) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Err(VmNativeCallError::Thrown(value)) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(VmNativeCallError::Internal(message)) => {
                            return Err(InterpreterError::NativeCall(message));
                        }
                    }
                }

                if !runtime.objects.is_callable(callee) {
                    let error = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }

                if matches!(runtime.objects.kind(callee), Ok(HeapValueKind::Closure))
                    && runtime
                        .objects
                        .closure_flags(callee)
                        .is_ok_and(|flags| flags.is_class_constructor())
                {
                    let error = runtime
                        .alloc_type_error("Class constructor cannot be invoked without 'new'")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }

                // §27.3.3.1 — Generator function call: create a generator object
                // instead of executing the body.
                // Spec: <https://tc39.es/ecma262/#sec-generatorfunction-objects-call>
                if matches!(runtime.objects.kind(callee), Ok(HeapValueKind::Closure))
                    && runtime
                        .objects
                        .closure_flags(callee)
                        .is_ok_and(|flags| flags.is_generator())
                {
                    let callee_module = runtime.objects.closure_module(callee)?;
                    let callee_fn_index = runtime.objects.closure_callee(callee)?;
                    let gen_handle = runtime.alloc_generator(
                        callee_module,
                        callee_fn_index,
                        Some(callee),
                        arguments.clone(),
                    );
                    activation.write_bytecode_register(
                        function,
                        instruction.a(),
                        RegisterValue::from_object_handle(gen_handle.0),
                    )?;
                    activation.advance();
                    return Ok(StepOutcome::Continue);
                }

                if let Ok(HeapValueKind::BoundFunction) = runtime.objects.kind(callee) {
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;
                    match runtime.call_callable_for_accessor(Some(callee), receiver, &arguments) {
                        Ok(value) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Err(InterpreterError::UncaughtThrow(value)) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                        Err(error) => return Err(error),
                    }
                }

                // ES2024 §27.2.1.3 — Promise capability / combinator / finally functions.
                if matches!(
                    runtime.objects.kind(callee),
                    Ok(HeapValueKind::PromiseCapabilityFunction
                        | HeapValueKind::PromiseCombinatorElement
                        | HeapValueKind::PromiseFinallyFunction
                        | HeapValueKind::PromiseValueThunk)
                ) {
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;
                    match Self::invoke_host_function_handle(
                        runtime, callee, receiver, &arguments,
                    )? {
                        Completion::Return(value) => {
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Completion::Throw(value) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                    }
                }

                if let Some(host_function) = runtime.objects.host_function(callee)? {
                    match Self::invoke_host_function(
                        callee,
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
                    let (callee_module, mut callee_activation) = Self::prepare_closure_call(
                        module,
                        activation,
                        runtime,
                        instruction.b(),
                        instruction.c(),
                        call,
                    )?;
                    match self.run_completion_with_runtime(
                        &callee_module,
                        &mut callee_activation,
                        runtime,
                    )? {
                        Completion::Return(value) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            Ok(StepOutcome::Continue)
                        }
                        Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
                    }
                }
            }
            Opcode::CallSuper => {
                if !function.is_derived_constructor()
                    || !activation.metadata().flags().is_construct()
                {
                    let error = runtime.alloc_reference_error("'super' keyword unexpected here")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }

                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let Some(super_ctor) = runtime.objects.get_prototype(closure)? else {
                    let error = runtime.alloc_type_error("Super constructor is not available")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };
                let new_target = activation.construct_new_target().unwrap_or(closure);
                let argc = instruction.c();
                let mut arguments = Vec::with_capacity(usize::from(argc));
                for offset in 0..argc {
                    let value = activation
                        .read_bytecode_register(function, instruction.b().saturating_add(offset))?;
                    arguments.push(value);
                }

                match runtime.construct_callable(super_ctor, &arguments, new_target) {
                    Ok(this_value) => {
                        if function.frame_layout().receiver_slot().is_some() {
                            activation.set_receiver(function, this_value)?;
                        }
                        activation.write_bytecode_register(
                            function,
                            instruction.a(),
                            this_value,
                        )?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
            Opcode::CallSuperForward => {
                if !function.is_derived_constructor()
                    || !activation.metadata().flags().is_construct()
                {
                    let error = runtime.alloc_reference_error("'super' keyword unexpected here")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                }

                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let Some(super_ctor) = runtime.objects.get_prototype(closure)? else {
                    let error = runtime.alloc_type_error("Super constructor is not available")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error.0,
                    )));
                };
                let new_target = activation.construct_new_target().unwrap_or(closure);
                let param_count = function.frame_layout().parameter_count();
                let actual_argc = activation.metadata().argument_count();
                let mut arguments = Vec::with_capacity(usize::from(actual_argc));
                for offset in 0..actual_argc {
                    let value = if offset < param_count {
                        activation.read_bytecode_register(function, offset)?
                    } else {
                        *activation
                            .overflow_args
                            .get(usize::from(offset - param_count))
                            .ok_or(InterpreterError::RegisterOutOfBounds)?
                    };
                    arguments.push(value);
                }

                match runtime.construct_callable(super_ctor, &arguments, new_target) {
                    Ok(this_value) => {
                        if function.frame_layout().receiver_slot().is_some() {
                            activation.set_receiver(function, this_value)?;
                        }
                        activation.write_bytecode_register(
                            function,
                            instruction.a(),
                            this_value,
                        )?;
                        activation.advance();
                        Ok(StepOutcome::Continue)
                    }
                    Err(VmNativeCallError::Thrown(value)) => Ok(StepOutcome::Throw(value)),
                    Err(VmNativeCallError::Internal(message)) => {
                        Err(InterpreterError::NativeCall(message))
                    }
                }
            }
            Opcode::Jump => {
                let offset = instruction.immediate_i32();
                if offset < 0 {
                    self.check_interrupt()?;
                    runtime.gc_safepoint(activation.registers());
                }
                activation.jump_relative(offset)?;
                Ok(StepOutcome::Continue)
            }
            Opcode::JumpIfTrue => {
                let condition = activation.read_bytecode_register(function, instruction.a())?;
                if runtime.js_to_boolean(condition)? {
                    let offset = instruction.immediate_i32();
                    if offset < 0 {
                        self.check_interrupt()?;
                        runtime.gc_safepoint(activation.registers());
                    }
                    activation.jump_relative(offset)?;
                } else {
                    activation.advance();
                }
                Ok(StepOutcome::Continue)
            }
            Opcode::JumpIfFalse => {
                let condition = activation.read_bytecode_register(function, instruction.a())?;
                if runtime.js_to_boolean(condition)? {
                    activation.advance();
                } else {
                    let offset = instruction.immediate_i32();
                    if offset < 0 {
                        self.check_interrupt()?;
                        runtime.gc_safepoint(activation.registers());
                    }
                    activation.jump_relative(offset)?;
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
            Opcode::Await => {
                let dst_reg = instruction.a();
                let src_reg = instruction.b();
                let value = activation.read_bytecode_register(function, src_reg)?;

                // Check if the value is an already-settled promise.
                // If it's an object handle, look it up as a JsPromise.
                if let Some(handle_id) = value.as_object_handle() {
                    let handle = ObjectHandle(handle_id);
                    // Try to read as JsPromise from the typed heap.
                    if let Some(promise) = runtime.objects().get_promise(handle) {
                        match &promise.state {
                            crate::promise::PromiseState::Fulfilled(result) => {
                                // Already fulfilled — write result, continue.
                                let result = *result;
                                let abs =
                                    activation.resolve_bytecode_register(function, dst_reg)?;
                                activation.set_register(abs, result)?;
                                activation.advance();
                                return Ok(StepOutcome::Continue);
                            }
                            crate::promise::PromiseState::Rejected(reason) => {
                                // Already rejected — throw the reason.
                                let reason = *reason;
                                activation.advance();
                                return Ok(StepOutcome::Throw(reason));
                            }
                            crate::promise::PromiseState::Pending => {
                                // Pending — suspend.
                                let abs =
                                    activation.resolve_bytecode_register(function, dst_reg)?;
                                activation.advance();
                                return Ok(StepOutcome::Suspend {
                                    awaited_promise: handle,
                                    resume_register: abs,
                                });
                            }
                        }
                    }
                }

                // Not a promise — treat as immediately fulfilled with the value itself.
                let abs = activation.resolve_bytecode_register(function, dst_reg)?;
                activation.set_register(abs, value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            // §14.4 Yield — suspend generator and produce a value.
            // Spec: <https://tc39.es/ecma262/#sec-yield>
            Opcode::Yield => {
                let value = activation.read_bytecode_register(function, instruction.b())?;
                let resume_reg = instruction.a();
                // Advance PC past the Yield instruction so resume continues
                // at the next instruction.
                activation.advance();
                Ok(StepOutcome::GeneratorYield {
                    yielded_value: value,
                    resume_register: resume_reg,
                })
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
        receiver: RegisterValue,
        property: PropertyNameId,
    ) -> Result<RegisterValue, InterpreterError> {
        match runtime.property_lookup(handle, property)? {
            Some(lookup) => {
                if let Some(cache) = lookup.cache() {
                    frame_runtime.update_property_cache(function, pc, cache);
                }
                match lookup.value() {
                    PropertyValue::Data { value, .. } => Ok(value),
                    PropertyValue::Accessor { getter, .. } => {
                        runtime.call_callable_for_accessor(getter, receiver, &[])
                    }
                }
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
        receiver: RegisterValue,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        match runtime.objects.kind(handle)? {
            HeapValueKind::String => return Ok(false),
            HeapValueKind::Array => {
                return match runtime.property_lookup(handle, property)? {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Accessor { setter, .. } => {
                            let _ =
                                runtime.call_callable_for_accessor(setter, receiver, &[value])?;
                            Ok(true)
                        }
                        PropertyValue::Data { .. } => Ok(false),
                    },
                    None => Ok(false),
                };
            }
            _ => {}
        }

        match runtime.property_lookup(handle, property)? {
            Some(lookup) => {
                if let Some(cache) = lookup.cache() {
                    frame_runtime.update_property_cache(function, pc, cache);
                }
                match lookup.value() {
                    PropertyValue::Data { .. } if lookup.owner() == handle => {
                        if let Some(cache) = lookup.cache() {
                            let updated =
                                runtime.objects.set_cached(handle, property, value, cache)?;
                            if updated {
                                return Ok(true);
                            }
                        }
                        let cache = runtime.objects.set_property(handle, property, value)?;
                        frame_runtime.update_property_cache(function, pc, cache);
                        Ok(true)
                    }
                    PropertyValue::Data { .. } => Ok(false),
                    PropertyValue::Accessor { setter, .. } => {
                        let _ = runtime.call_callable_for_accessor(setter, receiver, &[value])?;
                        Ok(true)
                    }
                }
            }
            None => Ok(false),
        }
    }

    fn primitive_set_property(
        runtime: &mut RuntimeState,
        target: ObjectHandle,
        receiver: RegisterValue,
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        match runtime.property_lookup(target, property)? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Accessor { setter, .. } => {
                    let _ = runtime.call_callable_for_accessor(setter, receiver, &[value])?;
                    Ok(true)
                }
                PropertyValue::Data { .. } => Ok(false),
            },
            None => Ok(false),
        }
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
            None,
        )?;

        Ok(activation)
    }

    fn prepare_closure_call(
        caller_module: &Module,
        caller_activation: &Activation,
        runtime: &RuntimeState,
        callee_register: RegisterIndex,
        arg_start: RegisterIndex,
        call: ClosureCall,
    ) -> Result<(Module, Activation), InterpreterError> {
        let closure = caller_activation
            .read_bytecode_register(
                caller_module
                    .function(caller_activation.function_index())
                    .expect("activation function index must be valid"),
                callee_register,
            )?
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(InterpreterError::InvalidObjectValue)?;
        let module = runtime.objects.closure_module(closure)?;
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
        let caller_function = caller_module
            .function(caller_activation.function_index())
            .expect("activation function index must be valid");
        let parameter_range = callee.frame_layout().parameter_range();
        let actual_argc = call.argument_count();
        let copy_count = actual_argc.min(parameter_range.len());

        for offset in 0..copy_count {
            let value = caller_activation
                .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
            activation.set_register(parameter_range.start().saturating_add(offset), value)?;
        }

        // ES2024 §10.4.4: Preserve overflow arguments for CreateArguments opcode.
        if actual_argc > parameter_range.len() {
            for offset in parameter_range.len()..actual_argc {
                let value = caller_activation
                    .read_bytecode_register(caller_function, arg_start.saturating_add(offset))?;
                activation.overflow_args.push(value);
            }
        }

        Self::initialize_receiver(
            caller_function,
            caller_activation,
            callee,
            &mut activation,
            call.flags(),
            call.receiver(),
            None,
        )?;

        Ok((module, activation))
    }

    fn invoke_host_function(
        callable: ObjectHandle,
        caller_function: &Function,
        caller_activation: &Activation,
        runtime: &mut RuntimeState,
        host_function: HostFunctionId,
        arg_start: RegisterIndex,
        call: ClosureCall,
    ) -> Result<Completion, InterpreterError> {
        let construct_receiver = if call.flags().is_construct() {
            if !Self::is_host_function_constructible(runtime, host_function)? {
                return Err(InterpreterError::InvalidCallTarget);
            }
            Some(RegisterValue::from_object_handle(
                Self::allocate_construct_receiver(runtime, callable)?.0,
            ))
        } else {
            None
        };
        let receiver = Self::resolve_call_receiver(
            caller_function,
            caller_activation,
            call.flags(),
            call.receiver(),
            construct_receiver,
        )?;
        let arguments = Self::read_call_arguments(
            caller_function,
            caller_activation,
            arg_start,
            call.argument_count(),
        )?;
        let completion = Self::invoke_registered_host_function(
            runtime,
            host_function,
            receiver,
            &arguments,
            call.flags().is_construct(),
        )?;
        if let Some(default_receiver) = construct_receiver {
            Ok(Self::apply_construct_return_override(
                completion,
                default_receiver,
            ))
        } else {
            Ok(completion)
        }
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
        construct_receiver: Option<RegisterValue>,
    ) -> Result<RegisterValue, InterpreterError> {
        match receiver_register {
            Some(receiver_register) => {
                caller_activation.read_bytecode_register(caller_function, receiver_register.index())
            }
            None if flags.is_construct() => {
                Ok(construct_receiver.unwrap_or_else(RegisterValue::undefined))
            }
            None if flags.has_receiver() => Ok(RegisterValue::undefined()),
            None => Ok(RegisterValue::undefined()),
        }
    }

    fn invoke_host_function_handle(
        runtime: &mut RuntimeState,
        callable: ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<Completion, InterpreterError> {
        // ES2024 §10.4.1.1 [[Call]] — resolve bound function chain.
        if let Ok(HeapValueKind::BoundFunction) = runtime.objects.kind(callable) {
            let (target, bound_this, bound_args) =
                runtime.objects.bound_function_parts(callable)?;
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return Self::invoke_host_function_handle(runtime, target, bound_this, &full_args);
        }

        // ES2024 §27.2.1.3 — Promise capability resolve/reject functions.
        if let Ok(HeapValueKind::PromiseCapabilityFunction) = runtime.objects.kind(callable) {
            let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
            Self::invoke_promise_capability_function(runtime, callable, value)?;
            return Ok(Completion::Return(RegisterValue::undefined()));
        }

        // Promise combinator per-element / finally / value-thunk dispatch.
        match runtime.objects.kind(callable) {
            Ok(HeapValueKind::PromiseCombinatorElement) => {
                let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
                let result = Self::invoke_promise_combinator_element(runtime, callable, value)?;
                return Ok(Completion::Return(result));
            }
            Ok(HeapValueKind::PromiseFinallyFunction) => {
                let value = arguments.first().copied().unwrap_or(RegisterValue::undefined());
                match Self::invoke_promise_finally_function(runtime, callable, value) {
                    Ok(v) => return Ok(Completion::Return(v)),
                    Err(InterpreterError::UncaughtThrow(v)) => {
                        return Ok(Completion::Throw(v));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(HeapValueKind::PromiseValueThunk) => {
                if let Some((v, k)) = runtime.objects.promise_value_thunk_info(callable) {
                    match k {
                        crate::promise::PromiseFinallyKind::ThenFinally => {
                            return Ok(Completion::Return(v));
                        }
                        crate::promise::PromiseFinallyKind::CatchFinally => {
                            return Ok(Completion::Throw(v));
                        }
                    }
                }
            }
            _ => {}
        }

        let host_function = runtime
            .objects
            .host_function(callable)?
            .ok_or(InterpreterError::InvalidCallTarget)?;
        Self::invoke_registered_host_function(runtime, host_function, receiver, arguments, false)
    }

    fn invoke_registered_host_function(
        runtime: &mut RuntimeState,
        host_function: HostFunctionId,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
        is_construct: bool,
    ) -> Result<Completion, InterpreterError> {
        let descriptor = runtime
            .native_functions()
            .get(host_function)
            .cloned()
            .ok_or(InterpreterError::InvalidCallTarget)?;

        runtime.native_call_construct_stack.push(is_construct);
        let completion = match (descriptor.callback())(&receiver, arguments, runtime) {
            Ok(value) => Ok(Completion::Return(value)),
            Err(VmNativeCallError::Thrown(value)) => Ok(Completion::Throw(value)),
            Err(VmNativeCallError::Internal(message)) => Err(InterpreterError::NativeCall(message)),
        };
        runtime.native_call_construct_stack.pop();
        completion
    }

    fn initialize_receiver(
        caller_function: &Function,
        caller_activation: &Activation,
        callee_function: &Function,
        callee_activation: &mut Activation,
        flags: FrameFlags,
        receiver_register: Option<BytecodeRegister>,
        construct_receiver: Option<RegisterValue>,
    ) -> Result<(), InterpreterError> {
        let receiver = match receiver_register {
            Some(receiver_register) => caller_activation
                .read_bytecode_register(caller_function, receiver_register.index())?,
            None if flags.is_construct() => {
                construct_receiver.unwrap_or_else(RegisterValue::undefined)
            }
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

    fn allocate_construct_receiver(
        runtime: &mut RuntimeState,
        constructor: ObjectHandle,
    ) -> Result<ObjectHandle, InterpreterError> {
        let prototype_property = runtime.intern_property_name("prototype");
        let default_prototype = runtime.intrinsics().object_prototype();
        let prototype = runtime
            .ordinary_get(
                constructor,
                prototype_property,
                RegisterValue::from_object_handle(constructor.0),
            )
            .map_err(|error| match error {
                VmNativeCallError::Thrown(value) => InterpreterError::UncaughtThrow(value),
                VmNativeCallError::Internal(message) => InterpreterError::NativeCall(message),
            })?
            .as_object_handle()
            .map(ObjectHandle)
            .unwrap_or(default_prototype);
        Ok(runtime.alloc_object_with_prototype(Some(prototype)))
    }

    fn is_host_function_constructible(
        runtime: &RuntimeState,
        host_function: HostFunctionId,
    ) -> Result<bool, InterpreterError> {
        let descriptor = runtime
            .native_functions()
            .get(host_function)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        Ok(descriptor.slot_kind() == NativeSlotKind::Constructor)
    }

    fn apply_construct_return_override(
        completion: Completion,
        default_receiver: RegisterValue,
    ) -> Completion {
        match completion {
            Completion::Return(value) if value.as_object_handle().is_some() => {
                Completion::Return(value)
            }
            Completion::Return(_) => Completion::Return(default_receiver),
            Completion::Throw(value) => Completion::Throw(value),
        }
    }

    /// Core generator resume implementation.
    ///
    /// Called from `RuntimeState::resume_generator` to execute generator body
    /// until the next yield, return, or throw.
    fn resume_generator_impl(
        runtime: &mut RuntimeState,
        generator: ObjectHandle,
        sent_value: RegisterValue,
        resume_kind: crate::intrinsics::GeneratorResumeKind,
    ) -> Result<RegisterValue, VmNativeCallError> {
        use crate::intrinsics::GeneratorResumeKind;
        use crate::object::GeneratorState;

        let (module, function_index, closure_handle, arguments, saved_registers, resume_pc, resume_reg) =
            runtime
                .objects
                .generator_take_state(generator)
                .map_err(|e| {
                    VmNativeCallError::Internal(format!("generator take state: {e:?}").into())
                })?;

        let function = module.function(function_index).ok_or_else(|| {
            VmNativeCallError::Internal("generator function index invalid".into())
        })?;

        let register_count = function.frame_layout().register_count();
        let had_saved_registers = saved_registers.is_some();

        // Build the activation.
        let mut activation = if let Some(saved_regs) = saved_registers {
            // Resuming from a yield point — restore the saved registers.
            let mut act = Activation::with_context(
                function_index,
                register_count,
                FrameMetadata::default(),
                closure_handle,
            );
            act.restore_registers(&saved_regs);
            act.set_pc(resume_pc);

            match resume_kind {
                GeneratorResumeKind::Next => {
                    act.write_bytecode_register(function, resume_reg, sent_value)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator resume write: {e:?}").into(),
                            )
                        })?;
                }
                GeneratorResumeKind::Return => {
                    // For .return() on a yielded generator, mark completed.
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator set state: {e:?}").into(),
                            )
                        })?;
                    let result = runtime.create_iter_result(sent_value, true)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
                GeneratorResumeKind::Throw => {
                    // We will inject the throw at the first step.
                }
            }
            act
        } else {
            // SuspendedStart — first call to .next().
            // Set up arguments in the activation's parameter registers.
            match resume_kind {
                GeneratorResumeKind::Next => {
                    let mut act = Activation::with_context(
                        function_index,
                        register_count,
                        FrameMetadata::new(
                            arguments.len() as u16,
                            FrameFlags::empty(),
                        ),
                        closure_handle,
                    );
                    // Write arguments to parameter registers.
                    let param_count = function.frame_layout().parameter_count();
                    for (i, &arg) in arguments.iter().enumerate() {
                        if i >= param_count as usize {
                            break;
                        }
                        let _ = act.write_bytecode_register(function, i as u16, arg);
                    }
                    act
                }
                GeneratorResumeKind::Return => {
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator set state: {e:?}").into(),
                            )
                        })?;
                    let result = runtime.create_iter_result(sent_value, true)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
                GeneratorResumeKind::Throw => {
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator set state: {e:?}").into(),
                            )
                        })?;
                    return Err(VmNativeCallError::Thrown(sent_value));
                }
            }
        };

        let interp = Interpreter::new();
        let previous_module = runtime.enter_module(&module);
        let mut frame_runtime = FrameRuntimeState::new(function);

        // For Throw resume kind on a yielded generator, inject exception.
        let mut inject_throw =
            matches!(resume_kind, GeneratorResumeKind::Throw) && had_saved_registers;

        loop {
            activation.begin_step();

            if inject_throw {
                inject_throw = false;
                if interp.transfer_exception(function, &mut activation, sent_value) {
                    continue;
                }
                runtime.restore_module(previous_module);
                runtime
                    .objects
                    .set_generator_state(generator, GeneratorState::Completed)
                    .ok();
                return Err(VmNativeCallError::Thrown(sent_value));
            }

            let outcome = match interp.step(
                function,
                &module,
                &mut activation,
                runtime,
                &mut frame_runtime,
            ) {
                Ok(outcome) => outcome,
                Err(InterpreterError::UncaughtThrow(value)) => StepOutcome::Throw(value),
                Err(InterpreterError::TypeError(message)) => {
                    let error = runtime
                        .alloc_type_error(&message)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    StepOutcome::Throw(RegisterValue::from_object_handle(error.0))
                }
                Err(error) => {
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Err(VmNativeCallError::Internal(
                        format!("generator execution error: {error:?}").into(),
                    ));
                }
            };

            match outcome {
                StepOutcome::Continue => {
                    activation
                        .sync_written_open_upvalues(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                    activation
                        .refresh_open_upvalues_from_cells(runtime)
                        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                }
                StepOutcome::Return(return_value) => {
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    let result = runtime.create_iter_result(return_value, true)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
                StepOutcome::Throw(value) => {
                    if interp.transfer_exception(function, &mut activation, value) {
                        continue;
                    }
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Err(VmNativeCallError::Thrown(value));
                }
                StepOutcome::Suspend { .. } => {
                    runtime.restore_module(previous_module);
                    runtime
                        .objects
                        .set_generator_state(generator, GeneratorState::Completed)
                        .ok();
                    return Err(VmNativeCallError::Internal(
                        "await inside generator not yet supported".into(),
                    ));
                }
                StepOutcome::GeneratorYield {
                    yielded_value,
                    resume_register: yield_resume_reg,
                } => {
                    let saved_regs = activation.save_registers();
                    let pc = activation.pc();
                    runtime
                        .objects
                        .generator_save_state(generator, saved_regs, pc, yield_resume_reg)
                        .map_err(|e| {
                            VmNativeCallError::Internal(
                                format!("generator save state: {e:?}").into(),
                            )
                        })?;
                    runtime.restore_module(previous_module);
                    let result = runtime.create_iter_result(yielded_value, false)?;
                    return Ok(RegisterValue::from_object_handle(result.0));
                }
            }
        }
    }
}

/// ES spec 7.1.4.1 StringToNumber — parses a string to a number.
/// ES spec 7.1.6 ToInt32(argument).
pub(crate) fn f64_to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    // Step 3-5: modulo 2^32, then adjust to signed range.
    let i = (n.trunc() % 4_294_967_296.0) as i64;
    let i = if i < 0 { i + 4_294_967_296 } else { i };
    if i >= 2_147_483_648 {
        (i - 4_294_967_296) as i32
    } else {
        i as i32
    }
}

/// ES spec 7.1.7 ToUint32(argument).
pub(crate) fn f64_to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = (n.trunc() % 4_294_967_296.0) as i64;
    if i < 0 {
        (i + 4_294_967_296) as u32
    } else {
        i as u32
    }
}

fn parse_string_to_number(s: &str) -> f64 {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => trimmed.parse::<f64>().unwrap_or(f64::NAN),
    }
}

fn canonical_string_exotic_index(property_name: &str) -> Option<usize> {
    let index = property_name.parse::<u32>().ok()?;
    if index == u32::MAX || index.to_string() != property_name {
        return None;
    }
    Some(index as usize)
}

#[cfg(test)]
mod tests {
    use crate::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset};
    use crate::call::{CallSite, CallTable, ClosureCall, DirectCall};
    use crate::closure::{CaptureDescriptor, ClosureTable, ClosureTemplate, UpvalueId};
    use crate::deopt::DeoptTable;
    use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
    use crate::exception::ExceptionTable;
    use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
    use crate::float::FloatTable;
    use crate::frame::{FrameFlags, FrameLayout};
    use crate::intrinsics::WellKnownSymbol;
    use crate::module::{Function, FunctionIndex, FunctionSideTables, FunctionTables, Module};
    use crate::object::{HeapValueKind, ObjectHandle, PropertyValue};
    use crate::payload::{VmTrace, VmValueTracer};
    use crate::property::PropertyNameTable;
    use crate::source_map::SourceMap;
    use crate::string::StringTable;
    use crate::value::{RegisterValue, ValueError};

    use super::{Activation, ExecutionResult, Interpreter, InterpreterError, RuntimeState};

    fn inherited_accessor_getter(
        this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let receiver = this
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| VmNativeCallError::Internal("expected object receiver".into()))?;
        let backing = runtime.intern_property_name("__backing");
        match runtime.objects().get_property(receiver, backing) {
            Ok(Some(lookup)) => match lookup.value() {
                PropertyValue::Data { value, .. } => Ok(value),
                PropertyValue::Accessor { .. } => Ok(RegisterValue::undefined()),
            },
            Ok(None) => Ok(RegisterValue::undefined()),
            Err(error) => Err(VmNativeCallError::Internal(
                format!("getter lookup failed: {error:?}").into(),
            )),
        }
    }

    fn inherited_accessor_setter(
        this: &RegisterValue,
        args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let receiver = this
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| VmNativeCallError::Internal("expected object receiver".into()))?;
        let backing = runtime.intern_property_name("__backing");
        let value = args
            .first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined);
        runtime
            .objects_mut()
            .set_property(receiver, backing, value)
            .map_err(|error| {
                VmNativeCallError::Internal(format!("setter store failed: {error:?}").into())
            })?;
        Ok(RegisterValue::undefined())
    }

    fn host_constructor_returns_primitive(
        this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let receiver = this
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| VmNativeCallError::Internal("expected construct receiver".into()))?;
        let value = runtime.intern_property_name("value");
        runtime
            .objects_mut()
            .set_property(receiver, value, RegisterValue::from_i32(7))
            .map_err(|error| {
                VmNativeCallError::Internal(format!("constructor store failed: {error:?}").into())
            })?;
        Ok(RegisterValue::from_i32(1))
    }

    fn host_constructor_returns_object(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let object = runtime.alloc_object();
        let value = runtime.intern_property_name("value");
        runtime
            .objects_mut()
            .set_property(object, value, RegisterValue::from_i32(9))
            .map_err(|error| {
                VmNativeCallError::Internal(format!("constructor store failed: {error:?}").into())
            })?;
        Ok(RegisterValue::from_object_handle(object.0))
    }

    fn host_plain_method(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(RegisterValue::undefined())
    }

    fn host_echo_receiver(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(*this)
    }

    #[derive(Debug, Clone, PartialEq)]
    struct NativeCounterPayload {
        root: RegisterValue,
        shadow: Option<ObjectHandle>,
        calls: i32,
    }

    impl VmTrace for NativeCounterPayload {
        fn trace(&self, tracer: &mut dyn VmValueTracer) {
            self.root.trace(tracer);
            self.shadow.trace(tracer);
        }
    }

    #[derive(Default)]
    struct CollectingTracer {
        values: Vec<RegisterValue>,
    }

    impl VmValueTracer for CollectingTracer {
        fn mark_value(&mut self, value: RegisterValue) {
            self.values.push(value);
        }
    }

    fn native_payload_reads_root(
        this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let payload = runtime
            .native_payload_from_value::<NativeCounterPayload>(this)
            .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))?;
        Ok(payload.root)
    }

    fn native_payload_allocates_then_throws(
        this: &RegisterValue,
        args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let shadow = runtime.alloc_object();
        {
            let payload = runtime
                .native_payload_mut_from_value::<NativeCounterPayload>(this)
                .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))?;
            payload.calls = payload.calls.saturating_add(1);
            payload.shadow = Some(shadow);
        }

        for index in 0..64 {
            let _ = runtime.alloc_string(format!("payload-temp-{index}"));
            let _ = runtime.alloc_object();
        }

        Err(VmNativeCallError::Thrown(
            args.first()
                .copied()
                .unwrap_or_else(RegisterValue::undefined),
        ))
    }

    #[test]
    fn runtime_native_objects_expose_typed_payload_access() {
        let mut runtime = RuntimeState::new();
        let root = runtime.alloc_string("payload-root");
        let instance = runtime.alloc_native_object(NativeCounterPayload {
            root: RegisterValue::from_object_handle(root.0),
            shadow: None,
            calls: 0,
        });

        let payload = runtime
            .native_payload::<NativeCounterPayload>(instance)
            .expect("payload should downcast");
        assert_eq!(payload.root, RegisterValue::from_object_handle(root.0));
        assert_eq!(payload.calls, 0);

        let method = runtime.register_native_function(NativeFunctionDescriptor::method(
            "readRoot",
            0,
            native_payload_reads_root,
        ));
        let descriptor = runtime
            .native_functions()
            .get(method)
            .cloned()
            .expect("native descriptor should exist");
        let value = (descriptor.callback())(
            &RegisterValue::from_object_handle(instance.0),
            &[],
            &mut runtime,
        )
        .expect("native payload method should succeed");

        assert!(
            runtime
                .objects()
                .native_payload_id(instance)
                .expect("native payload lookup should succeed")
                .is_some()
        );
        assert_eq!(runtime.objects().kind(instance), Ok(HeapValueKind::Object));
        assert_eq!(
            runtime
                .objects()
                .strict_eq(value, RegisterValue::from_object_handle(root.0)),
            Ok(true)
        );
    }

    #[test]
    fn runtime_native_payload_tracing_survives_allocation_and_throw_pressure() {
        let mut runtime = RuntimeState::new();
        let root = runtime.alloc_string("root");
        let instance = runtime.alloc_native_object(NativeCounterPayload {
            root: RegisterValue::from_object_handle(root.0),
            shadow: None,
            calls: 0,
        });

        let thrower = runtime.register_native_function(NativeFunctionDescriptor::method(
            "explode",
            1,
            native_payload_allocates_then_throws,
        ));
        let descriptor = runtime
            .native_functions()
            .get(thrower)
            .cloned()
            .expect("throwing descriptor should exist");
        let thrown = RegisterValue::from_i32(9);
        let error = (descriptor.callback())(
            &RegisterValue::from_object_handle(instance.0),
            &[thrown],
            &mut runtime,
        )
        .expect_err("throwing callback should propagate abrupt completion");
        assert_eq!(error, VmNativeCallError::Thrown(thrown));

        let payload = runtime
            .native_payload::<NativeCounterPayload>(instance)
            .expect("payload should still be readable after throw");
        assert_eq!(payload.calls, 1);
        let shadow = payload
            .shadow
            .expect("throwing callback should store shadow root");

        let mut tracer = CollectingTracer::default();
        runtime
            .trace_native_payload_roots(&mut tracer)
            .expect("payload trace should succeed");
        assert!(
            tracer
                .values
                .contains(&RegisterValue::from_object_handle(root.0))
        );
        assert!(
            tracer
                .values
                .contains(&RegisterValue::from_object_handle(shadow.0))
        );
    }

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
                    FloatTable::default(),
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
                Instruction::load_undefined(BytecodeRegister::new(0)),
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
                    FloatTable::default(),
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
        assert!(matches!(result, Err(InterpreterError::UncaughtThrow(_))));
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
            Instruction::new_array(BytecodeRegister::new(2), 0),
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
                    FloatTable::default(),
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
                    FloatTable::default(),
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
                    FloatTable::default(),
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
                    FloatTable::default(),
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
                    FloatTable::default(),
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
                    FloatTable::default(),
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
        let layout = FrameLayout::new(0, 0, 0, 7).expect("frame layout should be valid");
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
                    BytecodeRegister::new(3),
                    crate::property::PropertyNameId(2),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(3),
                ),
                Instruction::eq(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(3),
                ),
                Instruction::ret(BytecodeRegister::new(6)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["Object", "create", "valueOf"]),
                    StringTable::default(),
                    FloatTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            1,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(1),
                        ))),
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(3),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Comparison),
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
        let layout = FrameLayout::new(0, 0, 0, 10).expect("frame layout should be valid");
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
                    BytecodeRegister::new(2),
                    crate::property::PropertyNameId(4),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(2),
                ),
                Instruction::load_string(BytecodeRegister::new(8), crate::string::StringId(0)),
                Instruction::eq(
                    BytecodeRegister::new(9),
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(8),
                ),
                Instruction::ret(BytecodeRegister::new(9)),
                Instruction::load_false(BytecodeRegister::new(9)),
                Instruction::ret(BytecodeRegister::new(9)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec![
                        "Math",
                        "abs",
                        "Function",
                        "isCallable",
                        "toString",
                    ]),
                    StringTable::new(vec!["function () { [native code] }"]),
                    FloatTable::default(),
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
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(2),
                        ))),
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
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Comparison),
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
    fn interpreter_set_property_creates_own_data_slot_when_property_is_inherited() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
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
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["value"]),
                    StringTable::default(),
                    FloatTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("inherited-data-set"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let prototype = runtime.alloc_object();
        let object = runtime.alloc_object_with_prototype(Some(prototype));
        let property = runtime.intern_property_name("value");
        runtime
            .objects_mut()
            .set_property(prototype, property, RegisterValue::from_i32(1))
            .expect("prototype data property should install");
        let registers = [RegisterValue::from_object_handle(object.0)];

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
        let object_lookup = runtime
            .objects()
            .get_property(object, property)
            .expect("receiver lookup should succeed")
            .expect("receiver value should exist");
        assert_eq!(object_lookup.owner(), object);
        assert_eq!(
            object_lookup.value(),
            PropertyValue::data(RegisterValue::from_i32(7))
        );
        let prototype_lookup = runtime
            .objects()
            .get_property(prototype, property)
            .expect("prototype lookup should succeed")
            .expect("prototype value should exist");
        assert_eq!(prototype_lookup.owner(), prototype);
        assert_eq!(
            prototype_lookup.value(),
            PropertyValue::data(RegisterValue::from_i32(1))
        );
    }

    #[test]
    fn interpreter_set_property_invokes_inherited_accessor_setter() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
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
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["value"]),
                    StringTable::default(),
                    FloatTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(
            Some("inherited-accessor-set"),
            vec![entry],
            FunctionIndex(0),
        )
        .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let prototype = runtime.alloc_object();
        let object = runtime.alloc_object_with_prototype(Some(prototype));
        let property = runtime.intern_property_name("value");
        let getter = runtime.register_native_function(NativeFunctionDescriptor::getter(
            "value",
            inherited_accessor_getter,
        ));
        let setter = runtime.register_native_function(NativeFunctionDescriptor::setter(
            "value",
            inherited_accessor_setter,
        ));
        let getter = runtime.alloc_host_function(getter);
        let setter = runtime.alloc_host_function(setter);
        runtime
            .objects_mut()
            .define_accessor(prototype, property, Some(getter), Some(setter))
            .expect("prototype accessor should install");
        let registers = [RegisterValue::from_object_handle(object.0)];

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
        let lookup = runtime
            .objects()
            .get_property(object, property)
            .expect("receiver accessor lookup should succeed")
            .expect("receiver accessor should resolve");
        assert_eq!(lookup.owner(), prototype);
        let backing = runtime.intern_property_name("__backing");
        let backing_lookup = runtime
            .objects()
            .get_property(object, backing)
            .expect("receiver backing lookup should succeed")
            .expect("setter should have created receiver backing slot");
        assert_eq!(backing_lookup.owner(), object);
        assert_eq!(
            backing_lookup.value(),
            PropertyValue::data(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_constructs_host_function_with_return_override_rules() {
        let layout = FrameLayout::new(0, 0, 0, 9).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::call_closure(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(8),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(2),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(8),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(4),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::load_i32(BytecodeRegister::new(6), 7),
                Instruction::eq(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(6),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(6), JumpOffset::new(4)),
                Instruction::load_i32(BytecodeRegister::new(7), 9),
                Instruction::eq(
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(7),
                ),
                Instruction::ret(BytecodeRegister::new(7)),
                Instruction::load_false(BytecodeRegister::new(7)),
                Instruction::ret(BytecodeRegister::new(7)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["value"]),
                    StringTable::default(),
                    FloatTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(true, true, false),
                        ))),
                        None,
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(true, true, false),
                        ))),
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(false, true, false),
                        ))),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Comparison),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Comparison),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("host-construct"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();

        let primitive_constructor =
            runtime.register_native_function(NativeFunctionDescriptor::constructor(
                "PrimitiveCtor",
                0,
                host_constructor_returns_primitive,
            ));
        let object_constructor = runtime.register_native_function(
            NativeFunctionDescriptor::constructor("ObjectCtor", 0, host_constructor_returns_object),
        );
        let primitive_constructor = runtime.alloc_host_function(primitive_constructor);
        let object_constructor = runtime.alloc_host_function(object_constructor);
        let registers = [
            RegisterValue::from_object_handle(primitive_constructor.0),
            RegisterValue::from_object_handle(object_constructor.0),
        ];

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
    fn interpreter_throws_type_error_on_non_constructible_host_function() {
        let layout = FrameLayout::new(0, 0, 0, 2).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::call_closure(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::ret(BytecodeRegister::new(1)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    FloatTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(true, true, false),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![FeedbackSlotLayout::new(
                    FeedbackSlotId(0),
                    FeedbackKind::Call,
                )]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("bad-construct"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let method = runtime.register_native_function(NativeFunctionDescriptor::method(
            "plain",
            0,
            host_plain_method,
        ));
        let method = runtime.alloc_host_function(method);
        let registers = [RegisterValue::from_object_handle(method.0)];

        let error = Interpreter::new()
            .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
            .expect_err("constructing a plain host method should fail");

        assert!(matches!(error, InterpreterError::UncaughtThrow(_)));
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
                    FloatTable::default(),
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
                    FloatTable::default(),
                    ClosureTable::new(vec![
                        None,
                        Some(ClosureTemplate::new(FunctionIndex(1), [])),
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
    fn interpreter_host_method_calls_preserve_symbol_primitive_receiver() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::call_closure(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(2),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    FloatTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(1),
                        ))),
                        None,
                    ]),
                ),
                FeedbackTableLayout::new(vec![FeedbackSlotLayout::new(
                    FeedbackSlotId(0),
                    FeedbackKind::Call,
                )]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("symbol-host-this"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let method = runtime.register_native_function(NativeFunctionDescriptor::method(
            "echoReceiver",
            0,
            host_echo_receiver,
        ));
        let method = runtime.alloc_host_function(method);
        let receiver = runtime
            .intrinsics()
            .well_known_symbol_value(WellKnownSymbol::ToPrimitive);
        let registers = [
            RegisterValue::from_object_handle(method.0),
            receiver,
            RegisterValue::undefined(),
        ];

        let result = Interpreter::new()
            .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
            .expect("host symbol receiver call should execute");

        assert_eq!(result.return_value(), receiver);
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
                    FloatTable::default(),
                    ClosureTable::new(vec![
                        None,
                        Some(ClosureTemplate::new(
                            FunctionIndex(1),
                            [CaptureDescriptor::Register(BytecodeRegister::new(0))],
                        )),
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
