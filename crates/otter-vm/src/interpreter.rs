//! Interpreter entry points for the new VM.

use core::any::Any;
use core::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::bytecode::{BytecodeRegister, Instruction, Opcode, ProgramCounter};
use crate::call::{ClosureCall, DirectCall};
use crate::closure::{ClosureTemplate, UpvalueId};
use crate::descriptors::{NativeSlotKind, VmNativeCallError};
use crate::feedback::{FeedbackKind, FeedbackSlotId};
use crate::float::FloatId;
use crate::frame::{FrameFlags, FrameMetadata, RegisterIndex};
use crate::host::{HostFunctionId, NativeFunctionRegistry};
use crate::intrinsics::{VmIntrinsics, box_boolean_object, box_number_object};
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
    pending_exception: Option<RegisterValue>,
    pc: ProgramCounter,
    registers: Box<[RegisterValue]>,
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
            pending_exception: None,
            pc: 0,
            registers: vec![RegisterValue::default(); usize::from(register_count)]
                .into_boxed_slice(),
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
    native_payloads: NativePayloadRegistry,
    microtasks: crate::microtask::MicrotaskQueue,
    timers: crate::event_loop::TimerRegistry,
    console_backend: Box<dyn crate::console::ConsoleBackend>,
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
            native_payloads: NativePayloadRegistry::new(),
            microtasks: crate::microtask::MicrotaskQueue::new(),
            timers: crate::event_loop::TimerRegistry::new(),
            console_backend: Box::new(crate::console::StdioConsoleBackend),
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

    /// Returns own property keys using the runtime-wide property-name registry.
    pub fn own_property_keys(
        &mut self,
        object: ObjectHandle,
    ) -> Result<Vec<PropertyNameId>, ObjectError> {
        self.objects
            .own_keys_with_registry(object, &mut self.property_names)
    }

    /// Returns an own property descriptor without prototype traversal.
    pub fn own_property_descriptor(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        if let Some(descriptor) = self.string_exotic_own_property(object, property)? {
            return Ok(Some(descriptor));
        }
        self.objects
            .own_property_descriptor(object, property, &self.property_names)
    }

    /// Returns a named property lookup using the runtime-wide property registry.
    pub fn property_lookup(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyLookup>, ObjectError> {
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

    fn string_exotic_own_property(
        &mut self,
        object: ObjectHandle,
        property: PropertyNameId,
    ) -> Result<Option<PropertyValue>, ObjectError> {
        let Some(string) = self.objects.string_value(object)? else {
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

    /// Allocates one string object with the runtime default prototype.
    pub fn alloc_string(&mut self, value: impl Into<Box<str>>) -> ObjectHandle {
        let prototype = self.intrinsics.string_prototype();
        let handle = self.objects.alloc_string(value);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("string prototype should exist");
        handle
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
        let handle = self.objects.alloc_closure(callee, upvalues, flags);
        self.objects
            .set_prototype(handle, Some(prototype))
            .expect("function prototype should exist");
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
                        PropertyAttributes::frozen(),
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
                PropertyValue::Accessor { getter, .. } => {
                    self.call_host_function(getter, receiver, &[])
                }
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
                PropertyValue::Data { .. } => {
                    let Some(receiver_handle) =
                        self.non_string_object_handle(receiver).map_err(|error| {
                            VmNativeCallError::Internal(
                                format!("ordinary set receiver check failed: {error:?}").into(),
                            )
                        })?
                    else {
                        return Ok(false);
                    };

                    if lookup.owner() == receiver_handle {
                        let cache = lookup.cache().ok_or_else(|| {
                            VmNativeCallError::Internal(
                                "receiver own-property lookup must carry cache metadata".into(),
                            )
                        })?;
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
                                        format!("ordinary set receiver fallback failed: {error:?}")
                                            .into(),
                                    )
                                })?;
                        }
                        return Ok(true);
                    }

                    self.objects
                        .set_property(receiver_handle, property, value)
                        .map_err(|error| {
                            VmNativeCallError::Internal(
                                format!("ordinary set receiver define failed: {error:?}").into(),
                            )
                        })?;
                    Ok(true)
                }
                PropertyValue::Accessor { setter, .. } => {
                    let _ = self.call_host_function(setter, receiver, &[value])?;
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
                self.objects
                    .set_property(receiver_handle, property, value)
                    .map_err(|error| {
                        VmNativeCallError::Internal(
                            format!("ordinary set receiver create failed: {error:?}").into(),
                        )
                    })?;
                Ok(true)
            }
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

    pub(crate) fn boxed_primitive_value(
        &mut self,
        handle: ObjectHandle,
    ) -> Result<Option<RegisterValue>, InterpreterError> {
        if let Some(value) = self.own_data_property(handle, STRING_DATA_SLOT)?
            && value.as_object_handle().is_some()
        {
            return Ok(Some(value));
        }
        if let Some(value) = self.own_data_property(handle, NUMBER_DATA_SLOT)? {
            return Ok(Some(value));
        }
        if let Some(value) = self.own_data_property(handle, BOOLEAN_DATA_SLOT)? {
            return Ok(Some(value));
        }
        Ok(None)
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

    fn property_base_object_handle(
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
        Err(InterpreterError::InvalidObjectValue)
    }

    fn coerce_loose_equality_primitive(
        &mut self,
        value: RegisterValue,
    ) -> Result<RegisterValue, InterpreterError> {
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };

        if let Some(primitive) = self.boxed_primitive_value(handle)? {
            return Ok(primitive);
        }

        Ok(value)
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
            if let Some(primitive) = self.string_wrapper_data(handle)?
                && let Some(string) = self.objects.string_value(primitive)?
            {
                return Ok(string.to_string().into_boxed_str());
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
        if let Some(number) = value.as_number() {
            return Ok(number);
        }
        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            if let Some(string) = self.objects.string_value(handle)? {
                return Ok(parse_string_to_number(string));
            }
            // ToPrimitive(value, number) for objects — try valueOf slot, then
            // boxed primitive unwrap. Full spec valueOf/toString dispatch is
            // not yet available, but this handles wrapper objects (Number, Boolean, String).
            if let Some(primitive) = self.boxed_primitive_value(handle)? {
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
        let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(value);
        };
        // String heap values are already primitive-ish — extract the string.
        if let Some(_string) = self.objects.string_value(handle)? {
            return Ok(value);
        }
        // Boxed primitive wrappers (Number, Boolean, String constructors).
        if let Some(primitive) = self.boxed_primitive_value(handle)? {
            return Ok(primitive);
        }
        // Default for plain objects: NaN.
        Ok(RegisterValue::from_number(f64::NAN))
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
    fn js_instance_of(
        &mut self,
        value: RegisterValue,
        constructor: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        // LHS must be an object.
        let Some(obj_handle) = value.as_object_handle().map(ObjectHandle) else {
            return Ok(false);
        };
        // RHS must be an object with a "prototype" property.
        let Some(ctor_handle) = constructor.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Right-hand side of instanceof is not an object".into(),
            ));
        };
        let proto_prop = self.intern_property_name("prototype");
        let proto_value = match self.objects.get_property(ctor_handle, proto_prop)? {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { value: v, .. } => v,
                PropertyValue::Accessor { .. } => RegisterValue::undefined(),
            },
            None => RegisterValue::undefined(),
        };
        let Some(proto_handle) = proto_value.as_object_handle().map(ObjectHandle) else {
            return Err(InterpreterError::TypeError(
                "Function has non-object prototype in instanceof check".into(),
            ));
        };
        // Walk the prototype chain of value.
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

    /// `in` operator — check if a string property exists on an object.
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
        let key_str = self.js_to_string(key)?;
        let property = self.intern_property_name(&key_str);
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
        let lhs_is_string = lhs
            .as_object_handle()
            .map(ObjectHandle)
            .map(|handle| {
                matches!(self.objects.kind(handle), Ok(HeapValueKind::String))
                    || matches!(self.string_wrapper_data(handle), Ok(Some(_)))
            })
            .unwrap_or(false);
        let rhs_is_string = rhs
            .as_object_handle()
            .map(ObjectHandle)
            .map(|handle| {
                matches!(self.objects.kind(handle), Ok(HeapValueKind::String))
                    || matches!(self.string_wrapper_data(handle), Ok(Some(_)))
            })
            .unwrap_or(false);

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
        } else if value.as_number().is_some() {
            "number"
        } else if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            match self.objects.kind(handle)? {
                HeapValueKind::String => "string",
                HeapValueKind::HostFunction
                | HeapValueKind::Closure
                | HeapValueKind::BoundFunction => "function",
                HeapValueKind::Object
                | HeapValueKind::Array
                | HeapValueKind::UpvalueCell
                | HeapValueKind::Iterator
                | HeapValueKind::Promise => "object",
            }
        } else {
            "undefined"
        };

        let string = self.alloc_string(kind);
        Ok(RegisterValue::from_object_handle(string.0))
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
        module: &Module,
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
                let result = interpreter.run_with_runtime(module, &mut activation, runtime)?;
                Ok(result.return_value())
            }
            _ => Err(InterpreterError::TypeError(
                format!("{kind:?} is not a function").into(),
            )),
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
                StepOutcome::Suspend { .. } => {
                    // Suspension not supported in feedback-collection mode.
                    return Err(InterpreterError::TypeError(
                        "await is not supported in this execution mode".into(),
                    ));
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
                StepOutcome::Suspend { .. } => {
                    // TODO: Capture SuspendedFrame and return to event loop.
                    // For now, async functions are not fully wired — treat as
                    // returning undefined (the result_promise will be settled
                    // when the event loop integrates suspend/resume).
                    return Ok(Completion::Return(RegisterValue::undefined()));
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
                let base = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(base)?;
                let value = activation.read_bytecode_register(function, instruction.b())?;

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
                    property,
                )?;
                activation.write_bytecode_register(function, instruction.a(), value)?;
                activation.advance();
                Ok(StepOutcome::Continue)
            }
            Opcode::SetIndex => {
                let pc = activation.pc();
                let base = activation.read_bytecode_register(function, instruction.a())?;
                let handle = runtime.property_base_object_handle(base)?;
                let key = activation.read_bytecode_register(function, instruction.b())?;
                let value = activation.read_bytecode_register(function, instruction.c())?;
                let property = runtime.computed_property_name(key)?;

                match runtime.objects.kind(handle)? {
                    HeapValueKind::Array => {
                        let property_name = runtime.property_names().get(property).unwrap_or("");
                        if let Some(index) = canonical_string_exotic_index(property_name) {
                            runtime.objects.set_index(handle, index, value)?;
                        } else {
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
            // V8-style LdaGlobal: load a global variable by name from the
            // global object (receiver r0).  Throws if not found.
            Opcode::GetGlobal => {
                let property = Self::resolve_property_name(function, runtime, instruction.b())?;
                let global = activation.receiver(function)?;
                let global_handle = global
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
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
                let global = activation.receiver(function)?;
                let global_handle = global
                    .as_object_handle()
                    .map(ObjectHandle)
                    .ok_or(InterpreterError::InvalidObjectValue)?;
                runtime
                    .objects
                    .set_property(global_handle, property, value)?;
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
                let result = runtime.js_instance_of(lhs, rhs)?;
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
                let result = runtime.js_has_property(key, object)?;
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

                // ES2024 §10.4.1.1 [[Call]] — resolve bound function before dispatch.
                if let Ok(HeapValueKind::BoundFunction) = runtime.objects.kind(callee) {
                    let receiver = Self::resolve_call_receiver(
                        caller_function,
                        activation,
                        call.flags(),
                        call.receiver(),
                        None,
                    )?;
                    let arguments = Self::read_call_arguments(
                        caller_function,
                        activation,
                        instruction.c(),
                        call.argument_count(),
                    )?;
                    match Self::invoke_host_function_handle(runtime, callee, receiver, &arguments)?
                    {
                        Completion::Return(value) => {
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            return Ok(StepOutcome::Continue);
                        }
                        Completion::Throw(value) => return Ok(StepOutcome::Throw(value)),
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
                    let (mut callee_activation, construct_receiver) = if call.flags().is_construct()
                    {
                        let (activation, receiver) = Self::prepare_construct_closure_call(
                            module,
                            activation,
                            runtime,
                            instruction.b(),
                            instruction.c(),
                            call,
                        )?;
                        (activation, Some(receiver))
                    } else {
                        (
                            Self::prepare_closure_call(
                                module,
                                activation,
                                runtime,
                                instruction.b(),
                                instruction.c(),
                                call,
                            )?,
                            None,
                        )
                    };
                    match self.run_completion_with_runtime(
                        module,
                        &mut callee_activation,
                        runtime,
                    )? {
                        Completion::Return(value) => {
                            let value = if let Some(default_receiver) = construct_receiver {
                                match Self::apply_construct_return_override(
                                    Completion::Return(value),
                                    default_receiver,
                                ) {
                                    Completion::Return(value) => value,
                                    Completion::Throw(_) => {
                                        unreachable!("return override cannot throw")
                                    }
                                }
                            } else {
                                value
                            };
                            activation.write_bytecode_register(function, instruction.a(), value)?;
                            activation.advance();
                            Ok(StepOutcome::Continue)
                        }
                        Completion::Throw(value) => Ok(StepOutcome::Throw(value)),
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
        match runtime.property_lookup(handle, property)? {
            Some(lookup) => {
                if let Some(cache) = lookup.cache() {
                    frame_runtime.update_property_cache(function, pc, cache);
                }
                match lookup.value() {
                    PropertyValue::Data { value, .. } => Ok(value),
                    PropertyValue::Accessor { getter, .. } => {
                        Self::invoke_accessor_getter(runtime, handle, getter)
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
        property: PropertyNameId,
        value: RegisterValue,
    ) -> Result<bool, InterpreterError> {
        match runtime.objects.kind(handle)? {
            HeapValueKind::String => return Ok(false),
            HeapValueKind::Array => {
                return match runtime.property_lookup(handle, property)? {
                    Some(lookup) => match lookup.value() {
                        PropertyValue::Accessor { setter, .. } => {
                            Self::invoke_accessor_setter(runtime, handle, setter, value)?;
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
                        let cache = lookup
                            .cache()
                            .expect("own-property lookup should yield a receiver cache");
                        Ok(runtime.objects.set_cached(handle, property, value, cache)?)
                    }
                    PropertyValue::Data { .. } => Ok(false),
                    PropertyValue::Accessor { setter, .. } => {
                        Self::invoke_accessor_setter(runtime, handle, setter, value)?;
                        Ok(true)
                    }
                }
            }
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

        Ok(activation)
    }

    fn prepare_construct_closure_call(
        module: &Module,
        caller_activation: &Activation,
        runtime: &mut RuntimeState,
        callee_register: RegisterIndex,
        arg_start: RegisterIndex,
        call: ClosureCall,
    ) -> Result<(Activation, RegisterValue), InterpreterError> {
        let caller_function = module
            .function(caller_activation.function_index())
            .expect("activation function index must be valid");
        let closure = caller_activation
            .read_bytecode_register(caller_function, callee_register)?
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or(InterpreterError::InvalidObjectValue)?;
        let callee_index = runtime.objects.closure_callee(closure)?;
        let callee = module
            .function(callee_index)
            .ok_or(InterpreterError::InvalidCallTarget)?;
        let default_receiver = Self::allocate_construct_receiver(runtime, closure)?;
        let default_receiver_value = RegisterValue::from_object_handle(default_receiver.0);
        let mut activation = Activation::with_context(
            callee_index,
            callee.frame_layout().register_count(),
            FrameMetadata::new(call.argument_count(), call.flags()),
            Some(closure),
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
            Some(default_receiver_value),
        )?;

        Ok((activation, default_receiver_value))
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
        let completion =
            Self::invoke_registered_host_function(runtime, host_function, receiver, &arguments)?;
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
        // ES2024 §10.4.1.1 [[Call]] — resolve bound function chain.
        if let Ok(HeapValueKind::BoundFunction) = runtime.objects.kind(callable) {
            let (target, bound_this, bound_args) =
                runtime.objects.bound_function_parts(callable)?;
            let mut full_args = bound_args;
            full_args.extend_from_slice(arguments);
            return Self::invoke_host_function_handle(runtime, target, bound_this, &full_args);
        }

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
        let prototype = match runtime
            .objects()
            .get_property(constructor, prototype_property)?
        {
            Some(lookup) => match lookup.value() {
                PropertyValue::Data { value, .. } => value
                    .as_object_handle()
                    .map(ObjectHandle)
                    .unwrap_or(default_prototype),
                PropertyValue::Accessor { getter, .. } => {
                    let value = Self::invoke_accessor_getter(runtime, constructor, getter)?;
                    value
                        .as_object_handle()
                        .map(ObjectHandle)
                        .unwrap_or(default_prototype)
                }
            },
            None => default_prototype,
        };
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
    use crate::closure::{ClosureTable, ClosureTemplate, UpvalueId};
    use crate::deopt::DeoptTable;
    use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
    use crate::exception::ExceptionTable;
    use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
    use crate::float::FloatTable;
    use crate::frame::{FrameFlags, FrameLayout};
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

        assert!(matches!(result, Err(InterpreterError::TypeError(_))));
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
                            0,
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
    fn interpreter_rejects_construct_on_non_constructible_host_function() {
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

        assert_eq!(error, InterpreterError::InvalidCallTarget);
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
                    FloatTable::default(),
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
