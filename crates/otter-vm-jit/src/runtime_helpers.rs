//! Runtime helper infrastructure for JIT → VM callbacks.
//!
//! Complex bytecode operations (property access, function calls, object
//! creation) cannot be inlined into JIT code because they need the full VM
//! context (GC, shapes, prototype chains). Instead, JIT code calls extern "C"
//! helper functions through Cranelift's import mechanism.
//!
//! # Architecture
//!
//! ```text
//! otter-vm-jit (defines types + signatures)
//!       ↑
//! otter-vm-core (implements helpers, constructs RuntimeHelpers)
//! ```
//!
//! The JIT crate defines [`RuntimeHelpers`] (function pointer table) and
//! [`JitContext`] (opaque runtime context). The VM core crate fills in the
//! actual helper implementations and constructs a `JitContext` before calling
//! JIT-compiled functions.
//!
//! # ABI
//!
//! All JIT-compiled functions have signature `extern "C" fn(*mut u8) -> i64`:
//! - Parameter: opaque context pointer (cast to `*mut JitContext` by helpers)
//! - Return: NaN-boxed i64 value
//!
//! All helper functions take `*mut u8` (ctx) as first argument, followed by
//! i64 operands, and return i64.

use cranelift_codegen::ir::{self, AbiParam, types};
use cranelift_jit::JITBuilder;
use cranelift_module::{FuncId, Linkage, Module};

use crate::JitError;

// ---------------------------------------------------------------------------
// Helper kind enumeration
// ---------------------------------------------------------------------------

/// Identifies a runtime helper function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HelperKind {
    /// `(ctx, const_idx) -> value`
    LoadConst = 0,
    /// `(ctx, name_idx, ic_idx) -> value`
    GetGlobal = 1,
    /// `(ctx, name_idx, value, ic_idx, is_decl) -> 0`
    SetGlobal = 2,
    /// `(ctx, obj, name_idx, ic_idx) -> value`
    GetPropConst = 3,
    /// `(ctx, obj, name_idx, value, ic_idx) -> 0`
    SetPropConst = 4,
    /// `(ctx, obj, key, ic_idx) -> value`
    GetProp = 5,
    /// `(ctx, obj, key, value, ic_idx) -> 0`
    SetProp = 6,
    /// `(ctx, callee, argc, argv_ptr) -> value`
    CallFunction = 7,
    /// `(ctx, func_idx) -> value`
    CreateClosure = 8,
    /// `(ctx) -> value`
    NewObject = 9,
    /// `(ctx, len) -> value`
    NewArray = 10,
    /// `(ctx, value) -> diverges`
    ThrowValue = 11,
    /// `(ctx, obj, idx, ic_idx) -> value`
    GetElem = 12,
    /// `(ctx, obj, idx, value, ic_idx) -> 0`
    SetElem = 13,
    /// `(ctx, obj, key, val) -> 0`
    DefineProperty = 14,
    /// `(ctx, obj, key) -> bool_value`
    DeleteProp = 15,
    /// `(ctx, idx) -> value`
    GetUpvalue = 16,
    /// `(ctx, idx, value) -> 0`
    SetUpvalue = 17,
    /// `(ctx) -> value`
    LoadThis = 18,
    /// `(ctx, lhs, rhs) -> value` — generic JS `+`
    GenericAdd = 19,
    /// `(ctx, lhs, rhs) -> value` — generic JS `-`
    GenericSub = 20,
    /// `(ctx, lhs, rhs) -> value` — generic JS `*`
    GenericMul = 21,
    /// `(ctx, lhs, rhs) -> value` — generic JS `/`
    GenericDiv = 22,
    /// `(ctx, lhs, rhs) -> value` — generic JS `%`
    GenericMod = 23,
    /// `(ctx, val) -> value` — generic JS unary `-`
    GenericNeg = 24,
    /// `(ctx, val) -> value` — generic JS `++` (increment)
    GenericInc = 25,
    /// `(ctx, val) -> value` — generic JS `--` (decrement)
    GenericDec = 26,
    /// `(ctx, lhs, rhs) -> value` — generic JS `<`
    GenericLt = 27,
    /// `(ctx, lhs, rhs) -> value` — generic JS `<=`
    GenericLe = 28,
    /// `(ctx, lhs, rhs) -> value` — generic JS `>`
    GenericGt = 29,
    /// `(ctx, lhs, rhs) -> value` — generic JS `>=`
    GenericGe = 30,
    /// `(ctx, lhs, rhs) -> value` — generic JS `==`
    GenericEq = 31,
    /// `(ctx, lhs, rhs) -> value` — generic JS `!=`
    GenericNeq = 32,
    /// `(ctx, lhs, rhs) -> value` — generic JS bitwise (op encoded in 3rd arg)
    GenericBitOp = 33,
    /// `(ctx, val) -> value` — generic JS `~` (bitwise NOT)
    GenericBitNot = 34,
    /// `(ctx, val) -> value` — generic JS `!` (logical NOT)
    GenericNot = 35,
}

/// Total number of helper kinds.
pub const HELPER_COUNT: usize = 36;

impl HelperKind {
    /// Symbol name used for Cranelift import resolution.
    pub fn symbol_name(self) -> &'static str {
        match self {
            Self::LoadConst => "otter_rt_load_const",
            Self::GetGlobal => "otter_rt_get_global",
            Self::SetGlobal => "otter_rt_set_global",
            Self::GetPropConst => "otter_rt_get_prop_const",
            Self::SetPropConst => "otter_rt_set_prop_const",
            Self::GetProp => "otter_rt_get_prop",
            Self::SetProp => "otter_rt_set_prop",
            Self::CallFunction => "otter_rt_call_function",
            Self::CreateClosure => "otter_rt_create_closure",
            Self::NewObject => "otter_rt_new_object",
            Self::NewArray => "otter_rt_new_array",
            Self::ThrowValue => "otter_rt_throw_value",
            Self::GetElem => "otter_rt_get_elem",
            Self::SetElem => "otter_rt_set_elem",
            Self::DefineProperty => "otter_rt_define_property",
            Self::DeleteProp => "otter_rt_delete_prop",
            Self::GetUpvalue => "otter_rt_get_upvalue",
            Self::SetUpvalue => "otter_rt_set_upvalue",
            Self::LoadThis => "otter_rt_load_this",
            Self::GenericAdd => "otter_rt_generic_add",
            Self::GenericSub => "otter_rt_generic_sub",
            Self::GenericMul => "otter_rt_generic_mul",
            Self::GenericDiv => "otter_rt_generic_div",
            Self::GenericMod => "otter_rt_generic_mod",
            Self::GenericNeg => "otter_rt_generic_neg",
            Self::GenericInc => "otter_rt_generic_inc",
            Self::GenericDec => "otter_rt_generic_dec",
            Self::GenericLt => "otter_rt_generic_lt",
            Self::GenericLe => "otter_rt_generic_le",
            Self::GenericGt => "otter_rt_generic_gt",
            Self::GenericGe => "otter_rt_generic_ge",
            Self::GenericEq => "otter_rt_generic_eq",
            Self::GenericNeq => "otter_rt_generic_neq",
            Self::GenericBitOp => "otter_rt_generic_bitop",
            Self::GenericBitNot => "otter_rt_generic_bitnot",
            Self::GenericNot => "otter_rt_generic_not",
        }
    }

    /// Number of parameters (INCLUDING the ctx pointer).
    pub fn param_count(self) -> usize {
        match self {
            Self::NewObject | Self::LoadThis => 1,
            Self::LoadConst | Self::NewArray | Self::ThrowValue | Self::CreateClosure
            | Self::GetUpvalue | Self::GenericNeg | Self::GenericInc | Self::GenericDec
            | Self::GenericBitNot | Self::GenericNot => 2,
            Self::GetGlobal | Self::DeleteProp | Self::SetUpvalue
            | Self::GenericAdd | Self::GenericSub | Self::GenericMul | Self::GenericDiv
            | Self::GenericMod | Self::GenericLt | Self::GenericLe | Self::GenericGt
            | Self::GenericGe | Self::GenericEq | Self::GenericNeq => 3,
            Self::GetPropConst | Self::GetProp | Self::CallFunction | Self::GetElem
            | Self::DefineProperty | Self::GenericBitOp => 4,
            Self::SetPropConst | Self::SetProp | Self::SetElem | Self::SetGlobal => 5,
        }
    }

    /// Number of return values (always 1 — i64).
    pub fn return_count(self) -> usize {
        1
    }

    /// Build the Cranelift IR signature for this helper.
    pub fn make_signature(self) -> ir::Signature {
        let call_conv = cranelift_codegen::isa::CallConv::SystemV;
        let mut sig = ir::Signature::new(call_conv);
        for _ in 0..self.param_count() {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..self.return_count() {
            sig.returns.push(AbiParam::new(types::I64));
        }
        sig
    }
}

// ---------------------------------------------------------------------------
// RuntimeHelpers — function pointer table
// ---------------------------------------------------------------------------

/// Table of runtime helper function pointers.
///
/// Each slot is `Option<*const u8>` — a type-erased function pointer.
/// The VM core fills these in before constructing [`crate::JitCompiler`].
///
/// When a slot is `None`, the corresponding bytecode instruction will be
/// rejected as unsupported during compilation.
#[derive(Clone)]
pub struct RuntimeHelpers {
    /// Function pointers indexed by [`HelperKind`].
    ptrs: [Option<*const u8>; HELPER_COUNT],
}

// SAFETY: function pointers are `Send + Sync` by nature.
unsafe impl Send for RuntimeHelpers {}
unsafe impl Sync for RuntimeHelpers {}

impl Default for RuntimeHelpers {
    fn default() -> Self {
        Self {
            ptrs: [None; HELPER_COUNT],
        }
    }
}

impl RuntimeHelpers {
    /// Create an empty helper table (all helpers unset).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a helper function pointer.
    ///
    /// # Safety
    ///
    /// The function pointer must have the correct `extern "C"` signature
    /// matching the [`HelperKind`] parameter conventions.
    pub unsafe fn set(&mut self, kind: HelperKind, ptr: *const u8) {
        self.ptrs[kind as usize] = Some(ptr);
    }

    /// Get a helper function pointer.
    pub fn get(&self, kind: HelperKind) -> Option<*const u8> {
        self.ptrs[kind as usize]
    }

    /// Register all non-None helper pointers as symbols on the JIT builder.
    pub fn register_symbols(&self, builder: &mut JITBuilder) {
        for i in 0..HELPER_COUNT {
            if let Some(ptr) = self.ptrs[i] {
                // SAFETY: HelperKind variants are 0..HELPER_COUNT-1
                let kind = unsafe { std::mem::transmute::<u8, HelperKind>(i as u8) };
                builder.symbol(kind.symbol_name(), ptr);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HelperFuncIds — module-level function declarations
// ---------------------------------------------------------------------------

/// Module-level function IDs for declared helper imports.
/// Created once per [`crate::JitCompiler`].
pub(crate) struct HelperFuncIds {
    ids: [Option<FuncId>; HELPER_COUNT],
}

impl HelperFuncIds {
    /// Declare all available helpers as imported functions on the module.
    pub fn declare<M: Module>(helpers: &RuntimeHelpers, module: &mut M) -> Result<Self, JitError> {
        let mut ids = [None; HELPER_COUNT];
        for (i, ptr) in helpers.ptrs.iter().enumerate() {
            if ptr.is_some() {
                let kind = unsafe { std::mem::transmute::<u8, HelperKind>(i as u8) };
                let sig = kind.make_signature();
                let func_id =
                    module.declare_function(kind.symbol_name(), Linkage::Import, &sig)?;
                ids[i] = Some(func_id);
            }
        }
        Ok(Self { ids })
    }

    /// Get the FuncId for a helper kind (None if not available).
    #[allow(dead_code)]
    pub fn get(&self, kind: HelperKind) -> Option<FuncId> {
        self.ids[kind as usize]
    }
}

// ---------------------------------------------------------------------------
// HelperRefs — per-function FuncRefs for calling helpers from IR
// ---------------------------------------------------------------------------

/// Per-compiled-function helper references.
/// Created for each function compilation by declaring module FuncIds
/// into the function's IR context.
pub(crate) struct HelperRefs {
    refs: [Option<ir::FuncRef>; HELPER_COUNT],
}

impl HelperRefs {
    /// Declare all available helpers into a function's IR.
    pub fn declare<M: Module>(
        func_ids: &HelperFuncIds,
        module: &mut M,
        func: &mut ir::Function,
    ) -> Self {
        let mut refs = [None; HELPER_COUNT];
        for (i, id) in func_ids.ids.iter().enumerate() {
            if let Some(func_id) = id {
                refs[i] = Some(module.declare_func_in_func(*func_id, func));
            }
        }
        Self { refs }
    }

    /// Get the FuncRef for a helper kind (None if not available).
    pub fn get(&self, kind: HelperKind) -> Option<ir::FuncRef> {
        self.refs[kind as usize]
    }

    /// Get the FuncRef, or return UnsupportedInstruction error.
    pub fn require(
        &self,
        kind: HelperKind,
        pc: usize,
        opcode_name: &str,
    ) -> Result<ir::FuncRef, JitError> {
        self.get(kind).ok_or_else(|| JitError::UnsupportedInstruction {
            pc,
            opcode: opcode_name.to_string(),
        })
    }
}
