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
    /// `(ctx, val) -> value` — JS `typeof val`
    TypeOf = 36,
    /// `(ctx, name_idx) -> value` — JS `typeof globalName` (no ReferenceError)
    TypeOfName = 37,
    /// `(ctx, lhs, rhs) -> value` — JS `**` exponentiation
    Pow = 38,
    /// `(ctx, local_idx) -> 0` — close an upvalue cell for a local variable
    CloseUpvalue = 39,
    /// `(ctx, callee, argc, argv_ptr) -> value` — JS `new Ctor(args)`
    Construct = 40,
    /// `(ctx, obj, method_name_idx, argc, argv_ptr, ic_idx) -> value`
    CallMethod = 41,
    /// `(ctx, callee, this_val, argc, argv_ptr) -> value`
    CallWithReceiver = 42,
    /// `(ctx, obj, key, argc, argv_ptr, ic_idx) -> value`
    CallMethodComputed = 43,
    /// `(ctx, val) -> value` — JS ToNumber
    ToNumber = 44,
    /// `(ctx, val) -> value` — JS ToString
    JsToString = 45,
    /// `(ctx, val) -> 0 or BAILOUT` — RequireObjectCoercible
    RequireCoercible = 46,
    /// `(ctx, lhs, rhs, ic_idx) -> value` — JS `instanceof`
    InstanceOf = 47,
    /// `(ctx, lhs, rhs, ic_idx) -> value` — JS `in` operator
    InOp = 48,
    /// `(ctx, name_idx, configurable) -> 0` — declare global var binding
    DeclareGlobalVar = 49,
    /// `(ctx, obj, key, func) -> 0` — define getter on object
    DefineGetter = 50,
    /// `(ctx, obj, key, func) -> 0` — define setter on object
    DefineSetter = 51,
    /// `(ctx, obj, key, val) -> 0` — define method (non-enumerable) on object
    DefineMethod = 52,
    /// `(ctx, dst_arr, src_arr) -> 0` — spread elements from src into dst array
    SpreadArray = 53,
    /// `(ctx, func_idx) -> value` — create closure from function index
    ClosureCreate = 54,
    /// `(ctx) -> value` — create arguments object for current function
    CreateArguments = 55,
    /// `(ctx, src) -> value` — get iterator (Symbol.iterator)
    GetIterator = 56,
    /// `(ctx, iter) -> value` — call iterator.next(), returns packed (value, done) pair
    IteratorNext = 57,
    /// `(ctx, iter) -> 0` — close iterator (call iterator.return())
    IteratorClose = 58,
    /// `(ctx, callee, argc, argv_ptr, spread) -> value` — call with spread args
    CallSpread = 59,
    /// `(ctx, callee, argc, argv_ptr, spread) -> value` — construct with spread args
    ConstructSpread = 60,
    /// `(ctx, obj, key, spread, ic_idx) -> value` — call method computed with spread
    CallMethodComputedSpread = 61,
    /// `(ctx, callee, argc, argv_ptr) -> value` — tail call (returns result directly)
    TailCallHelper = 62,
    /// `(ctx, catch_pc) -> 0` — TryStart (push try handler onto try_stack)
    TryStart = 63,
    /// `(ctx) -> 0` — TryEnd (pop try handler from try_stack)
    TryEnd = 64,
    /// `(ctx) -> value` — Catch (take pending exception value)
    CatchOp = 65,
    /// `(ctx, ctor, super_class, name_idx) -> value` — DefineClass
    DefineClass = 66,
    /// `(ctx) -> value` — GetSuper (reads home_object from ctx)
    GetSuper = 67,
    /// `(ctx, argc, argv_ptr) -> value` — CallSuper
    CallSuper = 68,
    /// `(ctx, name_idx) -> value` — GetSuperProp
    GetSuperProp = 69,
    /// `(ctx, func, obj) -> 0` — SetHomeObject
    SetHomeObject = 70,
    /// `(ctx) -> value` — CallSuperForward (forward all args to super constructor)
    CallSuperForward = 71,
    /// `(ctx, args_array) -> value` — CallSuperSpread
    CallSuperSpread = 72,
    /// `(ctx) -> BAILOUT` — Yield (suspension)
    YieldOp = 73,
    /// `(ctx) -> BAILOUT` — Await (suspension)
    AwaitOp = 74,
    /// `(ctx, func_idx) -> value` — AsyncClosure (create async function closure)
    AsyncClosure = 75,
    /// `(ctx, func_idx) -> value` — GeneratorClosure (create generator function closure)
    GeneratorClosure = 76,
    /// `(ctx, func_idx) -> value` — AsyncGeneratorClosure (create async generator closure)
    AsyncGeneratorClosure = 77,
    /// `(ctx, code_val) -> value` — CallEval
    CallEval = 78,
    /// `(ctx, module_name_idx) -> value` — Import via host hooks
    ImportOp = 79,
    /// `(ctx, export_name_idx, value) -> 0` — Export via host hooks
    ExportOp = 80,
    /// `(ctx, src) -> value` — GetAsyncIterator
    GetAsyncIterator = 81,
    /// `(ctx, target) -> value_or_undefined` — ForInNext via host hooks
    ForInNext = 82,
    /// `(obj, expected_shape, offset) -> value` — monomorphic property read (no ctx)
    GetPropMono = 83,
}

/// Total number of helper kinds.
pub const HELPER_COUNT: usize = 84;

/// Byte offset of `secondary_result` field in JitContext (`#[repr(C)]`).
/// Used by IteratorNext to return both value and done flag.
/// MUST match JitContext layout in otter-vm-core/src/jit_helpers.rs.
/// Layout: function_ptr(0) proto_epoch(8) interpreter(16) vm_ctx(24)
///         constants(32) upvalues_ptr(40) upvalue_count:u32(48) pad(52)
///         this_raw(56) callee_raw(64) home_object_raw(72) secondary_result(80)
///         bailout_reason(88) bailout_pc(96)
///         deopt_locals_ptr(104) deopt_locals_count:u32(112) pad(116)
///         deopt_regs_ptr(120) deopt_regs_count:u32(128)
pub const JIT_CTX_SECONDARY_RESULT_OFFSET: i32 = 80;

/// Byte offset of `bailout_reason` in JitContext (`#[repr(C)]`).
pub const JIT_CTX_BAILOUT_REASON_OFFSET: i32 = 88;

/// Byte offset of `bailout_pc` in JitContext (`#[repr(C)]`).
pub const JIT_CTX_BAILOUT_PC_OFFSET: i32 = 96;

/// Byte offset of `deopt_locals_ptr` in JitContext (`#[repr(C)]`).
pub const JIT_CTX_DEOPT_LOCALS_PTR_OFFSET: i32 = 104;

/// Byte offset of `deopt_locals_count` in JitContext (`#[repr(C)]`).
pub const JIT_CTX_DEOPT_LOCALS_COUNT_OFFSET: i32 = 112;

/// Byte offset of `deopt_regs_ptr` in JitContext (`#[repr(C)]`).
pub const JIT_CTX_DEOPT_REGS_PTR_OFFSET: i32 = 120;

/// Byte offset of `deopt_regs_count` in JitContext (`#[repr(C)]`).
pub const JIT_CTX_DEOPT_REGS_COUNT_OFFSET: i32 = 128;

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
            Self::TypeOf => "otter_rt_typeof",
            Self::TypeOfName => "otter_rt_typeof_name",
            Self::Pow => "otter_rt_pow",
            Self::CloseUpvalue => "otter_rt_close_upvalue",
            Self::Construct => "otter_rt_construct",
            Self::CallMethod => "otter_rt_call_method",
            Self::CallWithReceiver => "otter_rt_call_with_receiver",
            Self::CallMethodComputed => "otter_rt_call_method_computed",
            Self::ToNumber => "otter_rt_to_number",
            Self::JsToString => "otter_rt_to_string",
            Self::RequireCoercible => "otter_rt_require_coercible",
            Self::InstanceOf => "otter_rt_instanceof",
            Self::InOp => "otter_rt_in",
            Self::DeclareGlobalVar => "otter_rt_declare_global_var",
            Self::DefineGetter => "otter_rt_define_getter",
            Self::DefineSetter => "otter_rt_define_setter",
            Self::DefineMethod => "otter_rt_define_method",
            Self::SpreadArray => "otter_rt_spread_array",
            Self::ClosureCreate => "otter_rt_closure_create",
            Self::CreateArguments => "otter_rt_create_arguments",
            Self::GetIterator => "otter_rt_get_iterator",
            Self::IteratorNext => "otter_rt_iterator_next",
            Self::IteratorClose => "otter_rt_iterator_close",
            Self::CallSpread => "otter_rt_call_spread",
            Self::ConstructSpread => "otter_rt_construct_spread",
            Self::CallMethodComputedSpread => "otter_rt_call_method_computed_spread",
            Self::TailCallHelper => "otter_rt_tail_call",
            Self::TryStart => "otter_rt_try_start",
            Self::TryEnd => "otter_rt_try_end",
            Self::CatchOp => "otter_rt_catch",
            Self::DefineClass => "otter_rt_define_class",
            Self::GetSuper => "otter_rt_get_super",
            Self::CallSuper => "otter_rt_call_super",
            Self::GetSuperProp => "otter_rt_get_super_prop",
            Self::SetHomeObject => "otter_rt_set_home_object",
            Self::CallSuperForward => "otter_rt_call_super_forward",
            Self::CallSuperSpread => "otter_rt_call_super_spread",
            Self::YieldOp => "otter_rt_yield",
            Self::AwaitOp => "otter_rt_await",
            Self::AsyncClosure => "otter_rt_async_closure",
            Self::GeneratorClosure => "otter_rt_generator_closure",
            Self::AsyncGeneratorClosure => "otter_rt_async_generator_closure",
            Self::CallEval => "otter_rt_call_eval",
            Self::ImportOp => "otter_rt_import",
            Self::ExportOp => "otter_rt_export",
            Self::GetAsyncIterator => "otter_rt_get_async_iterator",
            Self::ForInNext => "otter_rt_for_in_next",
            Self::GetPropMono => "otter_rt_get_prop_mono",
        }
    }

    /// Number of parameters (INCLUDING the ctx pointer).
    pub fn param_count(self) -> usize {
        match self {
            Self::NewObject
            | Self::LoadThis
            | Self::CreateArguments
            | Self::TryEnd
            | Self::CatchOp
            | Self::GetSuper
            | Self::CallSuperForward
            | Self::YieldOp
            | Self::AwaitOp => 1,
            Self::ImportOp | Self::ForInNext => 2,
            Self::ExportOp => 3,
            Self::LoadConst
            | Self::NewArray
            | Self::ThrowValue
            | Self::CreateClosure
            | Self::GetUpvalue
            | Self::GenericNeg
            | Self::GenericInc
            | Self::GenericDec
            | Self::GenericBitNot
            | Self::GenericNot
            | Self::TypeOf
            | Self::TypeOfName
            | Self::CloseUpvalue
            | Self::ToNumber
            | Self::JsToString
            | Self::RequireCoercible
            | Self::ClosureCreate
            | Self::GetIterator
            | Self::IteratorClose
            | Self::IteratorNext
            | Self::GetSuperProp
            | Self::CallEval
            | Self::GetAsyncIterator
            | Self::CallSuperSpread
            | Self::TryStart
            | Self::AsyncClosure
            | Self::GeneratorClosure
            | Self::AsyncGeneratorClosure => 2,
            Self::GetGlobal
            | Self::DeleteProp
            | Self::SetUpvalue
            | Self::GenericAdd
            | Self::GenericSub
            | Self::GenericMul
            | Self::GenericDiv
            | Self::GenericMod
            | Self::GenericLt
            | Self::GenericLe
            | Self::GenericGt
            | Self::GenericGe
            | Self::GenericEq
            | Self::GenericNeq
            | Self::Pow
            | Self::DeclareGlobalVar
            | Self::SpreadArray
            | Self::SetHomeObject
            | Self::CallSuper
            | Self::GetPropMono => 3,
            Self::GetPropConst
            | Self::GetProp
            | Self::CallFunction
            | Self::GetElem
            | Self::DefineProperty
            | Self::DefineGetter
            | Self::DefineSetter
            | Self::DefineMethod
            | Self::GenericBitOp
            | Self::Construct
            | Self::InstanceOf
            | Self::InOp
            | Self::TailCallHelper
            | Self::DefineClass => 4,
            Self::SetPropConst
            | Self::SetProp
            | Self::SetElem
            | Self::SetGlobal
            | Self::CallWithReceiver
            | Self::CallSpread
            | Self::ConstructSpread
            | Self::CallMethodComputedSpread => 5,
            Self::CallMethod | Self::CallMethodComputed => 6,
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
                let func_id = module.declare_function(kind.symbol_name(), Linkage::Import, &sig)?;
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
        self.get(kind)
            .ok_or_else(|| JitError::UnsupportedInstruction {
                pc,
                opcode: opcode_name.to_string(),
            })
    }
}
