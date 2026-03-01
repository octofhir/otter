//! Function bytecode representation

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::instruction::Instruction;
use crate::operand::LocalIndex;

/// Threshold for marking a function as "hot" (candidate for JIT compilation)
pub const HOT_FUNCTION_THRESHOLD: u32 = 1000;

/// Number of IC hits before quickening an instruction.
/// After this many consistent type observations, the generic instruction
/// is replaced with a specialized variant (e.g., Add → AddInt32).
pub const QUICKENING_WARMUP: u32 = 4;

/// Maximum number of times a function can be recompiled by the JIT.
/// After this many recompilations (each triggered by accumulated bailouts),
/// the function is permanently deoptimized (blacklisted from JIT).
/// Follows V8's approach: each recompilation uses wider feedback, preventing deopt loops.
pub const MAX_RECOMPILATIONS: u32 = 3;

/// Result of recording a JIT bailout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BailoutAction {
    /// Below bailout threshold — continue using current JIT code.
    Continue,
    /// Bailout threshold crossed, JIT code invalidated.
    /// Function should be re-enqueued for compilation with updated feedback.
    Recompile,
    /// Recompilation budget exhausted — permanently deoptimized.
    PermanentDeopt,
}

/// Function flags
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFlags {
    /// Is this an async function
    pub is_async: bool,
    /// Is this a generator function
    pub is_generator: bool,
    /// Is this an arrow function
    pub is_arrow: bool,
    /// Does this function use `arguments`
    pub uses_arguments: bool,
    /// Does this function use `eval`
    pub uses_eval: bool,
    /// Is strict mode
    pub is_strict: bool,
    /// Is a constructor
    pub is_constructor: bool,
    /// Is a method
    pub is_method: bool,
    /// Is a getter
    pub is_getter: bool,
    /// Is a setter
    pub is_setter: bool,
    /// Has rest parameter (...args)
    pub has_rest: bool,
    /// Is a derived constructor (class extends)
    pub is_derived: bool,
    /// Has simple parameter list (no rest, no defaults, no destructuring)
    /// Per ES2024 §15.1.1: determines whether arguments object is mapped
    pub has_simple_parameters: bool,
}

/// Upvalue capture mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpvalueCapture {
    /// Capture from parent's local variable
    Local(LocalIndex),
    /// Capture from parent's upvalue (transitive capture)
    Upvalue(LocalIndex),
}

/// State of an Inline Cache (IC) for property access
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InlineCacheState {
    /// Initial state: no information cached
    #[default]
    Uninitialized,
    /// Monomorphic state: single shape and offset cached
    Monomorphic {
        /// The shape identifier of the cached object
        shape_id: u64,
        /// The offset into the object's properties
        offset: u32,
    },
    /// Polymorphic state: multiple shapes and offsets cached (up to 4)
    Polymorphic {
        /// Number of cached entries (1-4)
        count: u8,
        /// Array of (shape_id, offset) pairs
        entries: [(u64, u32); 4],
    },
    /// Megamorphic state: too many shapes seen, fallback to slow path
    Megamorphic,
    /// Quickened arithmetic specialization type
    ArithmeticFastPath(ArithmeticType),
}

/// Type of arithmetic fast path
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArithmeticType {
    /// Both operands are Int32
    Int32,
    /// Both operands are Numbers (f64) or mixed numeric
    Number,
    /// Both operands are Strings (for Add only)
    String,
}

/// Type flags for value type observations (used for type feedback)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeFlags {
    /// Has seen undefined
    pub seen_undefined: bool,
    /// Has seen null
    pub seen_null: bool,
    /// Has seen boolean
    pub seen_boolean: bool,
    /// Has seen int32 (small integer)
    pub seen_int32: bool,
    /// Has seen number (float64)
    pub seen_number: bool,
    /// Has seen string
    pub seen_string: bool,
    /// Has seen object
    pub seen_object: bool,
    /// Has seen function
    pub seen_function: bool,
}

impl TypeFlags {
    /// Check if this is monomorphic (only one type seen)
    pub fn is_monomorphic(&self) -> bool {
        let count = self.seen_undefined as u8
            + self.seen_null as u8
            + self.seen_boolean as u8
            + self.seen_int32 as u8
            + self.seen_number as u8
            + self.seen_string as u8
            + self.seen_object as u8
            + self.seen_function as u8;
        count == 1
    }

    /// Check if this is polymorphic (2-4 types seen)
    pub fn is_polymorphic(&self) -> bool {
        let count = self.seen_undefined as u8
            + self.seen_null as u8
            + self.seen_boolean as u8
            + self.seen_int32 as u8
            + self.seen_number as u8
            + self.seen_string as u8
            + self.seen_object as u8
            + self.seen_function as u8;
        (2..=4).contains(&count)
    }

    /// Check if only int32 has been seen
    #[inline]
    pub fn is_int32_only(&self) -> bool {
        self.seen_int32
            && !self.seen_number
            && !self.seen_undefined
            && !self.seen_null
            && !self.seen_boolean
            && !self.seen_string
            && !self.seen_object
            && !self.seen_function
    }

    /// Check if only number (f64) has been seen
    #[inline]
    pub fn is_number_only(&self) -> bool {
        self.seen_number
            && !self.seen_int32
            && !self.seen_undefined
            && !self.seen_null
            && !self.seen_boolean
            && !self.seen_string
            && !self.seen_object
            && !self.seen_function
    }

    /// Check if only numeric types (int32 or number) have been seen
    #[inline]
    pub fn is_numeric_only(&self) -> bool {
        (self.seen_int32 || self.seen_number)
            && !self.seen_undefined
            && !self.seen_null
            && !self.seen_boolean
            && !self.seen_string
            && !self.seen_object
            && !self.seen_function
    }

    /// Record seeing undefined
    #[inline]
    pub fn observe_undefined(&mut self) {
        self.seen_undefined = true;
    }

    /// Record seeing null
    #[inline]
    pub fn observe_null(&mut self) {
        self.seen_null = true;
    }

    /// Record seeing boolean
    #[inline]
    pub fn observe_boolean(&mut self) {
        self.seen_boolean = true;
    }

    /// Record seeing int32
    #[inline]
    pub fn observe_int32(&mut self) {
        self.seen_int32 = true;
    }

    /// Record seeing number (f64)
    #[inline]
    pub fn observe_number(&mut self) {
        self.seen_number = true;
    }

    /// Record seeing string
    #[inline]
    pub fn observe_string(&mut self) {
        self.seen_string = true;
    }

    /// Record seeing object
    #[inline]
    pub fn observe_object(&mut self) {
        self.seen_object = true;
    }

    /// Record seeing function
    #[inline]
    pub fn observe_function(&mut self) {
        self.seen_function = true;
    }
}

/// Metadata for a single IC slot (instruction-level profiling)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstructionMetadata {
    /// Inline cache state for property access
    pub ic_state: InlineCacheState,
    /// Hit count for this IC site
    pub hit_count: u32,
    /// Type observations for values at this site
    pub type_observations: TypeFlags,
    /// Prototype epoch at cache time (for invalidation).
    /// When prototype chains change, the global proto_epoch is bumped.
    /// IC entries are invalidated when their cached proto_epoch doesn't match.
    pub proto_epoch: u64,
}

impl InstructionMetadata {
    /// Create a new uninitialized metadata entry
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a cache hit
    #[inline]
    pub fn record_hit(&mut self) {
        self.hit_count = self.hit_count.saturating_add(1);
    }

    /// Transition IC to monomorphic state
    pub fn transition_to_monomorphic(&mut self, shape_id: u64, offset: u32) {
        self.ic_state = InlineCacheState::Monomorphic { shape_id, offset };
    }

    /// Transition IC to monomorphic state with proto epoch
    pub fn transition_to_monomorphic_with_epoch(
        &mut self,
        shape_id: u64,
        offset: u32,
        proto_epoch: u64,
    ) {
        self.ic_state = InlineCacheState::Monomorphic { shape_id, offset };
        self.proto_epoch = proto_epoch;
    }

    /// Transition IC to megamorphic state
    pub fn transition_to_megamorphic(&mut self) {
        self.ic_state = InlineCacheState::Megamorphic;
    }

    /// Check if proto_epoch matches (for invalidation)
    #[inline]
    pub fn proto_epoch_matches(&self, current_epoch: u64) -> bool {
        self.proto_epoch == current_epoch
    }

    /// Update proto_epoch
    #[inline]
    pub fn update_proto_epoch(&mut self, epoch: u64) {
        self.proto_epoch = epoch;
    }
}

/// Thread-confined mutable vector for inline cache feedback data.
///
/// Wraps `UnsafeCell<Vec<InstructionMetadata>>` to provide zero-overhead
/// interior mutability. The VM is single-threaded (one isolate = one thread),
/// so no synchronization is needed.
#[allow(unsafe_code)]
pub struct FeedbackVector {
    inner: std::cell::UnsafeCell<Vec<InstructionMetadata>>,
}

// SAFETY: FeedbackVector is only accessed from a single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
#[allow(unsafe_code)]
unsafe impl Send for FeedbackVector {}
#[allow(unsafe_code)]
unsafe impl Sync for FeedbackVector {}

#[allow(unsafe_code)]
impl FeedbackVector {
    /// Create a new feedback vector from a vec.
    pub fn new(vec: Vec<InstructionMetadata>) -> Self {
        Self {
            inner: std::cell::UnsafeCell::new(vec),
        }
    }

    /// Get a shared reference to the inner vector.
    #[inline]
    pub fn read(&self) -> &Vec<InstructionMetadata> {
        // SAFETY: VM is single-threaded, no concurrent mutable access
        unsafe { &*self.inner.get() }
    }

    /// Get a mutable reference to the inner vector.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn write(&self) -> &mut Vec<InstructionMetadata> {
        // SAFETY: VM is single-threaded, no concurrent access
        unsafe { &mut *self.inner.get() }
    }
}

impl Clone for FeedbackVector {
    fn clone(&self) -> Self {
        Self::new(self.read().clone())
    }
}

impl std::fmt::Debug for FeedbackVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.read().fmt(f)
    }
}

impl Serialize for FeedbackVector {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FeedbackVector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let vec = Vec::<InstructionMetadata>::deserialize(deserializer)?;
        Ok(Self::new(vec))
    }
}

/// Thread-confined mutable vector for bytecode instructions.
///
/// Wraps `UnsafeCell<Vec<Instruction>>` to provide zero-overhead
/// interior mutability, enabling runtime bytecode quickening
/// (replacing generic instructions with specialized variants).
#[allow(unsafe_code)]
pub struct BytecodeVector {
    inner: std::cell::UnsafeCell<Vec<Instruction>>,
}

// SAFETY: BytecodeVector is only accessed from a single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
#[allow(unsafe_code)]
unsafe impl Send for BytecodeVector {}
#[allow(unsafe_code)]
unsafe impl Sync for BytecodeVector {}

#[allow(unsafe_code)]
impl BytecodeVector {
    /// Create a new bytecode vector from a vec.
    pub fn new(vec: Vec<Instruction>) -> Self {
        Self {
            inner: std::cell::UnsafeCell::new(vec),
        }
    }

    /// Get a shared reference to the inner vector.
    #[inline]
    pub fn read(&self) -> &Vec<Instruction> {
        // SAFETY: VM is single-threaded, no concurrent mutable access
        unsafe { &*self.inner.get() }
    }

    /// Get a mutable reference to the inner vector.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub fn write(&self) -> &mut Vec<Instruction> {
        // SAFETY: VM is single-threaded, no concurrent access
        unsafe { &mut *self.inner.get() }
    }
}

impl Clone for BytecodeVector {
    fn clone(&self) -> Self {
        Self::new(self.read().clone())
    }
}

impl std::fmt::Debug for BytecodeVector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.read().fmt(f)
    }
}

impl Serialize for BytecodeVector {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.read().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BytecodeVector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let vec = Vec::<Instruction>::deserialize(deserializer)?;
        Ok(Self::new(vec))
    }
}

/// A bytecode function
#[derive(Debug, Serialize, Deserialize)]
pub struct Function {
    /// Function name (empty for anonymous)
    pub name: Option<String>,

    /// Number of parameters (not including rest)
    pub param_count: u8,

    /// Number of local variables (including params)
    pub local_count: u16,

    /// Number of registers needed
    pub register_count: u16,

    /// Function flags
    pub flags: FunctionFlags,

    /// Upvalue captures
    pub upvalues: Vec<UpvalueCapture>,

    /// Bytecode instructions (mutable at runtime for quickening)
    pub instructions: BytecodeVector,

    /// Feedback vector for Inline Caches (mutable at runtime)
    /// Contains IC state and statistics for each IC site
    pub feedback_vector: FeedbackVector,

    /// Source location mapping (instruction index -> source offset)
    pub source_map: Option<SourceMap>,

    /// Parameter names (for debugging)
    pub param_names: Vec<String>,

    /// Local variable names (for debugging)
    pub local_names: Vec<String>,

    /// Call count for hot function detection (atomic for thread safety)
    /// Used to determine when a function should be JIT compiled
    #[serde(skip)]
    pub call_count: AtomicU32,

    /// Whether this function has been marked as hot (candidate for JIT)
    #[serde(skip)]
    pub is_hot: std::sync::atomic::AtomicBool,

    /// Number of times JIT code bailed out to the interpreter (resets on recompilation)
    #[serde(skip)]
    pub bailout_count: AtomicU32,

    /// Number of times JIT has recompiled this function (monotonically increasing).
    /// After MAX_RECOMPILATIONS, permanently deoptimize.
    #[serde(skip)]
    pub recompilation_count: AtomicU32,

    /// Whether this function has been deoptimized (JIT code invalidated permanently)
    #[serde(skip)]
    pub is_deoptimized: std::sync::atomic::AtomicBool,

    /// Cached entry pointer for compiled JIT code (0 means not compiled).
    #[serde(skip)]
    pub jit_entry_ptr: AtomicUsize,

    /// For derived constructors: index of an inner function that initializes
    /// instance fields. Called on `this` after `super()` returns.
    pub field_init_func: Option<u32>,
}

impl Function {
    /// Create a new function builder
    pub fn builder() -> FunctionBuilder {
        FunctionBuilder::new()
    }

    /// Get the function name or `<anonymous>`
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("<anonymous>")
    }

    /// Check if function is async
    #[inline]
    pub fn is_async(&self) -> bool {
        self.flags.is_async
    }

    /// Check if function is a generator
    #[inline]
    pub fn is_generator(&self) -> bool {
        self.flags.is_generator
    }

    /// Check if function is async generator
    #[inline]
    pub fn is_async_generator(&self) -> bool {
        self.flags.is_async && self.flags.is_generator
    }

    /// Check if function is an arrow function
    #[inline]
    pub fn is_arrow(&self) -> bool {
        self.flags.is_arrow
    }

    /// Check if function is in strict mode
    #[inline]
    pub fn is_strict(&self) -> bool {
        self.flags.is_strict
    }

    /// Increment the call count and check if the function should be marked as hot.
    /// Returns `true` if this call caused the function to become hot (first time crossing threshold).
    #[inline]
    pub fn record_call(&self) -> bool {
        self.record_call_with_threshold(HOT_FUNCTION_THRESHOLD)
    }

    /// Increment call count using a caller-provided hot threshold.
    /// Returns `true` if this call caused the function to become hot.
    #[inline]
    pub fn record_call_with_threshold(&self, hot_threshold: u32) -> bool {
        let hot_threshold = hot_threshold.max(1);

        // Once hot, avoid atomic RMW on every subsequent call in hot loops.
        if self.is_hot.load(Ordering::Relaxed) {
            return false;
        }

        let prev_count = self.call_count.fetch_add(1, Ordering::Relaxed);
        let new_count = prev_count.saturating_add(1);

        // Check if we just crossed the hot threshold
        if new_count >= hot_threshold && prev_count < hot_threshold {
            // Try to mark as hot (only succeeds once)
            if self
                .is_hot
                .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return true; // First time becoming hot
            }
        }
        false
    }

    /// Get the current call count
    #[inline]
    pub fn get_call_count(&self) -> u32 {
        self.call_count.load(Ordering::Relaxed)
    }

    /// Check if this function has been marked as hot
    #[inline]
    pub fn is_hot_function(&self) -> bool {
        self.is_hot.load(Ordering::Relaxed)
    }

    /// Manually mark this function as hot (e.g., for testing or forced JIT)
    #[inline]
    pub fn mark_hot(&self) {
        self.is_hot.store(true, Ordering::Release);
    }

    /// Record a JIT bailout. Returns the action taken:
    /// - `BailoutAction::Continue` — below threshold, keep running JIT code
    /// - `BailoutAction::Recompile` — threshold crossed, JIT code invalidated, ready for recompilation
    /// - `BailoutAction::PermanentDeopt` — exhausted recompilations, permanently deoptimized
    #[inline]
    pub fn record_bailout(&self, deopt_threshold: u32) -> BailoutAction {
        let prev = self.bailout_count.fetch_add(1, Ordering::Relaxed);
        let new_count = prev.saturating_add(1);
        if new_count >= deopt_threshold {
            // Threshold crossed — invalidate JIT code
            self.jit_entry_ptr.store(0, Ordering::Release);

            let recomp = self.recompilation_count.fetch_add(1, Ordering::Relaxed) + 1;
            if recomp >= MAX_RECOMPILATIONS {
                // Exhausted recompilation budget — permanent deoptimization
                self.is_deoptimized.store(true, Ordering::Release);
                BailoutAction::PermanentDeopt
            } else {
                // Reset bailout count for the next compilation cycle.
                // Feedback vector retains the wider type observations.
                self.bailout_count.store(0, Ordering::Relaxed);
                BailoutAction::Recompile
            }
        } else {
            BailoutAction::Continue
        }
    }

    /// Get the current bailout count
    #[inline]
    pub fn get_bailout_count(&self) -> u32 {
        self.bailout_count.load(Ordering::Relaxed)
    }

    /// Check if this function has been deoptimized
    #[inline]
    pub fn is_deoptimized(&self) -> bool {
        self.is_deoptimized.load(Ordering::Relaxed)
    }

    /// Check if this function needs recompilation.
    /// True when the function is hot, was deopt'd for recompilation (not permanently),
    /// and has no current JIT code.
    #[inline]
    pub fn needs_recompilation(&self) -> bool {
        self.is_hot_function()
            && !self.is_deoptimized()
            && self.recompilation_count.load(Ordering::Relaxed) > 0
            && self.jit_entry_ptr() == 0
    }

    /// Get the current recompilation count
    #[inline]
    pub fn get_recompilation_count(&self) -> u32 {
        self.recompilation_count.load(Ordering::Relaxed)
    }

    /// Get cached JIT entry pointer (0 means no compiled code cached).
    #[inline]
    pub fn jit_entry_ptr(&self) -> usize {
        self.jit_entry_ptr.load(Ordering::Acquire)
    }

    /// Cache JIT entry pointer for lock-free execution fast path.
    #[inline]
    pub fn set_jit_entry_ptr(&self, ptr: usize) {
        self.jit_entry_ptr.store(ptr, Ordering::Release);
    }

    /// Clear cached JIT entry pointer.
    #[inline]
    pub fn clear_jit_entry_ptr(&self) {
        self.jit_entry_ptr.store(0, Ordering::Release);
    }

    /// Replace the instruction at `pc` with a quickened variant.
    /// Used by the interpreter to specialize hot instructions based on
    /// observed types (e.g., Add → AddInt32 when only int32s seen).
    #[inline]
    pub fn quicken_instruction(&self, pc: usize, new_instr: Instruction) {
        let bytecode = self.instructions.write();
        if pc < bytecode.len() {
            bytecode[pc] = new_instr;
        }
    }
}

impl Clone for Function {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            param_count: self.param_count,
            local_count: self.local_count,
            register_count: self.register_count,
            flags: self.flags,
            upvalues: self.upvalues.clone(),
            instructions: BytecodeVector::new(self.instructions.read().clone()),
            feedback_vector: self.feedback_vector.clone(),
            source_map: self.source_map.clone(),
            param_names: self.param_names.clone(),
            local_names: self.local_names.clone(),
            // Clone resets call statistics (new clone starts fresh)
            call_count: AtomicU32::new(0),
            is_hot: std::sync::atomic::AtomicBool::new(false),
            // Preserve bailout/recompilation state — deoptimized functions must stay deoptimized
            bailout_count: AtomicU32::new(self.bailout_count.load(Ordering::Relaxed)),
            recompilation_count: AtomicU32::new(self.recompilation_count.load(Ordering::Relaxed)),
            is_deoptimized: std::sync::atomic::AtomicBool::new(
                self.is_deoptimized.load(Ordering::Relaxed),
            ),
            jit_entry_ptr: AtomicUsize::new(0),
            field_init_func: self.field_init_func,
        }
    }
}

/// Builder for creating functions
#[derive(Debug, Default)]
pub struct FunctionBuilder {
    name: Option<String>,
    param_count: u8,
    local_count: u16,
    register_count: u16,
    flags: FunctionFlags,
    upvalues: Vec<UpvalueCapture>,
    instructions: Vec<Instruction>,
    feedback_vector: Vec<InstructionMetadata>,
    source_map: Option<SourceMap>,
    param_names: Vec<String>,
    local_names: Vec<String>,
}

impl FunctionBuilder {
    /// Create a new function builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Set function name
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set parameter count
    pub fn param_count(mut self, count: u8) -> Self {
        self.param_count = count;
        self
    }

    /// Set local variable count
    pub fn local_count(mut self, count: u16) -> Self {
        self.local_count = count;
        self
    }

    /// Set register count
    pub fn register_count(mut self, count: u16) -> Self {
        self.register_count = count;
        self
    }

    /// Set flags
    pub fn flags(mut self, flags: FunctionFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Mark as async
    pub fn is_async(mut self, value: bool) -> Self {
        self.flags.is_async = value;
        self
    }

    /// Mark as generator
    pub fn is_generator(mut self, value: bool) -> Self {
        self.flags.is_generator = value;
        self
    }

    /// Mark as arrow function
    pub fn is_arrow(mut self, value: bool) -> Self {
        self.flags.is_arrow = value;
        self
    }

    /// Mark as strict mode
    pub fn is_strict(mut self, value: bool) -> Self {
        self.flags.is_strict = value;
        self
    }

    /// Add upvalue capture
    pub fn upvalue(mut self, capture: UpvalueCapture) -> Self {
        self.upvalues.push(capture);
        self
    }

    /// Set all upvalue captures
    pub fn upvalues(mut self, upvalues: Vec<UpvalueCapture>) -> Self {
        self.upvalues = upvalues;
        self
    }

    /// Set all instructions
    pub fn instructions(mut self, instructions: Vec<Instruction>) -> Self {
        self.instructions = instructions;
        self
    }

    /// Add a single instruction
    pub fn instruction(mut self, instruction: Instruction) -> Self {
        self.instructions.push(instruction);
        self
    }

    /// Set feedback vector size
    pub fn feedback_vector_size(mut self, size: usize) -> Self {
        self.feedback_vector = vec![InstructionMetadata::new(); size];
        self
    }

    /// Set source map
    pub fn source_map(mut self, source_map: SourceMap) -> Self {
        self.source_map = Some(source_map);
        self
    }

    /// Add parameter name
    pub fn param_name(mut self, name: impl Into<String>) -> Self {
        self.param_names.push(name.into());
        self
    }

    /// Set all parameter names
    pub fn param_names(mut self, names: Vec<String>) -> Self {
        self.param_names = names;
        self
    }

    /// Add local variable name
    pub fn local_name(mut self, name: impl Into<String>) -> Self {
        self.local_names.push(name.into());
        self
    }

    /// Set all local variable names
    pub fn local_names(mut self, names: Vec<String>) -> Self {
        self.local_names = names;
        self
    }

    /// Build the function
    pub fn build(self) -> Function {
        Function {
            name: self.name,
            param_count: self.param_count,
            local_count: self.local_count,
            register_count: self.register_count,
            flags: self.flags,
            upvalues: self.upvalues,
            instructions: BytecodeVector::new(self.instructions),
            feedback_vector: FeedbackVector::new(self.feedback_vector),
            source_map: self.source_map,
            param_names: self.param_names,
            local_names: self.local_names,
            call_count: AtomicU32::new(0),
            is_hot: std::sync::atomic::AtomicBool::new(false),
            bailout_count: AtomicU32::new(0),
            recompilation_count: AtomicU32::new(0),
            is_deoptimized: std::sync::atomic::AtomicBool::new(false),
            jit_entry_ptr: AtomicUsize::new(0),
            field_init_func: None,
        }
    }
}

/// Source location mapping
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceMap {
    /// Entries mapping instruction index to source location
    pub entries: Vec<SourceMapEntry>,
}

/// A single source map entry
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SourceMapEntry {
    /// Instruction index
    pub instruction_index: u32,
    /// Source file offset (bytes)
    pub source_offset: u32,
    /// Line number (1-indexed)
    pub line: u32,
    /// Column number (1-indexed)
    pub column: u32,
}

impl SourceMap {
    /// Create a new empty source map
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a mapping entry
    pub fn add(&mut self, instruction_index: u32, source_offset: u32, line: u32, column: u32) {
        self.entries.push(SourceMapEntry {
            instruction_index,
            source_offset,
            line,
            column,
        });
    }

    /// Find source location for instruction index
    pub fn find(&self, instruction_index: u32) -> Option<&SourceMapEntry> {
        // Binary search for the entry
        let idx = self
            .entries
            .binary_search_by_key(&instruction_index, |e| e.instruction_index);

        match idx {
            Ok(i) => Some(&self.entries[i]),
            Err(i) if i > 0 => Some(&self.entries[i - 1]),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operand::Register;

    #[test]
    fn test_function_builder() {
        let func = Function::builder()
            .name("add")
            .param_count(2)
            .local_count(2)
            .register_count(3)
            .is_strict(true)
            .instruction(Instruction::Add {
                dst: Register(0),
                lhs: Register(1),
                rhs: Register(2),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        assert_eq!(func.display_name(), "add");
        assert_eq!(func.param_count, 2);
        assert_eq!(func.instructions.read().len(), 2);
        assert!(func.is_strict());
    }

    #[test]
    fn test_source_map() {
        let mut map = SourceMap::new();
        map.add(0, 0, 1, 1);
        map.add(5, 20, 2, 5);
        map.add(10, 50, 3, 1);

        assert_eq!(map.find(0).unwrap().line, 1);
        assert_eq!(map.find(5).unwrap().line, 2);
        assert_eq!(map.find(7).unwrap().line, 2); // Between entries
        assert_eq!(map.find(10).unwrap().line, 3);
    }

    #[test]
    fn record_call_stops_counting_after_hot_transition() {
        let func = Function::builder().name("hot").build();

        for _ in 0..(HOT_FUNCTION_THRESHOLD - 1) {
            assert!(!func.record_call());
        }

        assert_eq!(func.get_call_count(), HOT_FUNCTION_THRESHOLD - 1);
        assert!(!func.is_hot_function());

        assert!(func.record_call());
        assert!(func.is_hot_function());
        assert_eq!(func.get_call_count(), HOT_FUNCTION_THRESHOLD);

        // After hot transition, `record_call` should avoid extra RMWs.
        for _ in 0..16 {
            assert!(!func.record_call());
        }

        assert_eq!(func.get_call_count(), HOT_FUNCTION_THRESHOLD);
    }

    #[test]
    fn record_call_with_custom_threshold_marks_hot_earlier() {
        let func = Function::builder().name("hot-custom").build();

        assert!(!func.record_call_with_threshold(3));
        assert!(!func.record_call_with_threshold(3));
        assert_eq!(func.get_call_count(), 2);
        assert!(!func.is_hot_function());

        assert!(func.record_call_with_threshold(3));
        assert!(func.is_hot_function());
        assert_eq!(func.get_call_count(), 3);

        // Once hot, no further call-count increments on fast path.
        assert!(!func.record_call_with_threshold(3));
        assert_eq!(func.get_call_count(), 3);
    }

    #[test]
    fn jit_entry_ptr_clears_on_deopt() {
        let func = Function::builder().name("jit").build();
        func.set_jit_entry_ptr(0xDEAD_BEEF);
        assert_eq!(func.jit_entry_ptr(), 0xDEAD_BEEF);

        // First 2 bailouts are below threshold (threshold=3)
        assert_eq!(func.record_bailout(3), BailoutAction::Continue);
        assert_eq!(func.record_bailout(3), BailoutAction::Continue);
        assert_eq!(func.jit_entry_ptr(), 0xDEAD_BEEF);

        // Third bailout triggers recompilation (first recompilation cycle)
        assert_eq!(func.record_bailout(3), BailoutAction::Recompile);
        assert_eq!(func.jit_entry_ptr(), 0);
        assert!(!func.is_deoptimized()); // Not permanently deoptimized yet

        // Reset entry pointer for next cycle
        func.set_jit_entry_ptr(0xDEAD_BEEF);

        // Second recompilation cycle
        for _ in 0..2 {
            assert_eq!(func.record_bailout(3), BailoutAction::Continue);
        }
        assert_eq!(func.record_bailout(3), BailoutAction::Recompile);
        assert_eq!(func.jit_entry_ptr(), 0);
        assert!(!func.is_deoptimized());

        func.set_jit_entry_ptr(0xDEAD_BEEF);

        // Third (final) recompilation cycle => permanent deopt
        for _ in 0..2 {
            assert_eq!(func.record_bailout(3), BailoutAction::Continue);
        }
        assert_eq!(func.record_bailout(3), BailoutAction::PermanentDeopt);
        assert_eq!(func.jit_entry_ptr(), 0);
        assert!(func.is_deoptimized());
    }
}
