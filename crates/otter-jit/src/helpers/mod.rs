//! Runtime helper definitions and registration for JIT cold exits.
//!
//! Helpers are `extern "C"` functions called by JIT code for operations
//! that can't be inlined (property access, function calls, object creation, etc.).
//!
//! The helper function pointers are provided by the VM core at initialization
//! time via `HelperTable`. The JIT crate only defines the interfaces.

use cranelift_codegen::ir::{types, AbiParam, Signature};
use cranelift_codegen::isa::CallConv;

/// A table of runtime helper function pointers.
///
/// Filled in by `otter-vm-core` at JIT initialization time.
/// The JIT codegen reads from this table to register helpers in the JITModule.
#[derive(Debug, Clone)]
pub struct HelperTable {
    pub entries: Vec<HelperEntry>,
}

/// A single helper entry: name, function pointer, and signature metadata.
#[derive(Debug, Clone)]
pub struct HelperEntry {
    /// Human-readable name (for debugging / disassembly).
    pub name: &'static str,
    /// The actual function pointer.
    pub ptr: *const u8,
    /// Number of i64 arguments (excluding the JitContext* first arg).
    pub arg_count: usize,
    /// Whether this helper may allocate (triggers GC).
    pub may_gc: bool,
    /// Whether this helper may call back into JS (VM reentry).
    pub may_reenter: bool,
}

// SAFETY: Function pointers are stable addresses.
unsafe impl Send for HelperEntry {}
unsafe impl Sync for HelperEntry {}

/// Identifies a helper in the HelperTable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HelperId {
    // Property access
    GetPropConst = 0,
    SetPropConst = 1,
    GetProp = 2,
    SetProp = 3,
    DeleteProp = 4,

    // Element access
    GetElem = 5,
    SetElem = 6,

    // Calls
    CallFunction = 7,
    CallMethod = 8,
    Construct = 9,

    // Arithmetic (generic, type-coercing)
    GenericAdd = 10,
    GenericSub = 11,
    GenericMul = 12,
    GenericDiv = 13,
    GenericMod = 14,
    GenericNeg = 15,
    GenericInc = 16,
    GenericDec = 17,
    GenericPow = 18,

    // Comparison (generic)
    GenericEq = 19,
    GenericStrictEq = 20,
    GenericLt = 21,
    GenericLe = 22,
    GenericGt = 23,
    GenericGe = 24,

    // Object creation
    NewObject = 25,
    NewArray = 26,
    CreateClosure = 27,
    CreateArguments = 28,
    DefineProperty = 29,

    // Globals
    GetGlobal = 30,
    SetGlobal = 31,

    // Upvalues
    GetUpvalue = 32,
    SetUpvalue = 33,
    CloseUpvalue = 34,

    // Type operations
    TypeOf = 35,
    InstanceOf = 36,
    In = 37,
    ToNumber = 38,
    ToStringOp = 39,
    RequireCoercible = 40,
    IsTruthy = 41,

    // Iteration
    GetIterator = 42,
    IteratorNext = 43,
    IteratorClose = 44,

    // Exception
    TryStart = 45,
    TryEnd = 46,
    Throw = 47,

    // GC
    WriteBarrier = 48,

    // Constants
    LoadConst = 49,

    // Misc
    LoadThis = 50,
    Spread = 51,
}

impl HelperId {
    /// Number of i64 arguments this helper takes (excluding JitContext*).
    pub fn arg_count(self) -> usize {
        match self {
            // (ctx, obj, name_idx, ic_index) -> value
            HelperId::GetPropConst => 3,
            // (ctx, obj, name_idx, val, ic_index) -> 0
            HelperId::SetPropConst => 4,
            // (ctx, obj, key, ic_index) -> value
            HelperId::GetProp => 3,
            // (ctx, obj, key, val, ic_index) -> 0
            HelperId::SetProp => 4,
            // (ctx, obj, key) -> bool
            HelperId::DeleteProp => 2,
            // (ctx, arr, idx, ic_index) -> value
            HelperId::GetElem => 3,
            // (ctx, arr, idx, val, ic_index) -> 0
            HelperId::SetElem => 4,
            // (ctx, callee, argc) -> value
            HelperId::CallFunction => 2,
            // (ctx, obj, name_idx, argc, ic_index) -> value
            HelperId::CallMethod => 4,
            // (ctx, callee, argc) -> value
            HelperId::Construct => 2,
            // Binary ops: (ctx, lhs, rhs) -> value
            HelperId::GenericAdd
            | HelperId::GenericSub
            | HelperId::GenericMul
            | HelperId::GenericDiv
            | HelperId::GenericMod
            | HelperId::GenericPow
            | HelperId::GenericEq
            | HelperId::GenericStrictEq
            | HelperId::GenericLt
            | HelperId::GenericLe
            | HelperId::GenericGt
            | HelperId::GenericGe => 2,
            // Unary ops: (ctx, val) -> value
            HelperId::GenericNeg
            | HelperId::GenericInc
            | HelperId::GenericDec
            | HelperId::TypeOf
            | HelperId::ToNumber
            | HelperId::ToStringOp
            | HelperId::RequireCoercible
            | HelperId::IsTruthy
            | HelperId::GetIterator
            | HelperId::IteratorNext
            | HelperId::IteratorClose
            | HelperId::Throw
            | HelperId::WriteBarrier
            | HelperId::Spread => 1,
            // (ctx, lhs, rhs, ic_index) -> value
            HelperId::InstanceOf | HelperId::In => 3,
            // (ctx) -> value
            HelperId::NewObject
            | HelperId::CreateArguments
            | HelperId::TryEnd
            | HelperId::LoadThis => 0,
            // (ctx, len) -> value
            HelperId::NewArray => 1,
            // (ctx, func_idx) -> value
            HelperId::CreateClosure => 1,
            // (ctx, obj, key, val) -> 0
            HelperId::DefineProperty => 3,
            // (ctx, name_idx, ic_index) -> value
            HelperId::GetGlobal => 2,
            // (ctx, name_idx, val, ic_index) -> 0
            HelperId::SetGlobal => 3,
            // (ctx, idx) -> value
            HelperId::GetUpvalue | HelperId::SetUpvalue | HelperId::CloseUpvalue => 1,
            // (ctx, catch_offset) -> 0
            HelperId::TryStart => 1,
            // (ctx, const_idx) -> value
            HelperId::LoadConst => 1,
        }
    }

    /// Build a Cranelift signature for this helper.
    pub fn signature(self, call_conv: CallConv, pointer_type: types::Type) -> Signature {
        let mut sig = Signature::new(call_conv);
        // First arg: JitContext pointer
        sig.params.push(AbiParam::new(pointer_type));
        // Extra args: all i64 (NaN-boxed values or integer indices)
        for _ in 0..self.arg_count() {
            sig.params.push(AbiParam::new(types::I64));
        }
        // Return: i64 (NaN-boxed value or status)
        sig.returns.push(AbiParam::new(types::I64));
        sig
    }
}

impl HelperTable {
    /// Create an empty table.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register a helper.
    pub fn register(
        &mut self,
        id: HelperId,
        name: &'static str,
        ptr: *const u8,
        may_gc: bool,
        may_reenter: bool,
    ) {
        let idx = id as usize;
        // Ensure the table is large enough.
        while self.entries.len() <= idx {
            self.entries.push(HelperEntry {
                name: "<unregistered>",
                ptr: std::ptr::null(),
                arg_count: 0,
                may_gc: false,
                may_reenter: false,
            });
        }
        self.entries[idx] = HelperEntry {
            name,
            ptr,
            arg_count: id.arg_count(),
            may_gc,
            may_reenter,
        };
    }

    /// Get a helper entry by ID.
    pub fn get(&self, id: HelperId) -> Option<&HelperEntry> {
        self.entries.get(id as usize).filter(|e| !e.ptr.is_null())
    }
}

impl Default for HelperTable {
    fn default() -> Self {
        Self::new()
    }
}
