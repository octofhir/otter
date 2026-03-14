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

#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// `(ctx, callee, argc, argv_ptr, expected_func_ptr) -> value` — monomorphic call
    CallMono = 84,
    /// `(ctx, obj, idx) -> value`
    GetElemInt = 85,
    /// `(ctx, callee, argc, argv_ptr, ffi_call_info_ptr) -> value` — FFI direct call
    CallFfi = 86,
    /// `(obj, expected_shape, offset, value) -> 0_or_bailout` — monomorphic property write (no ctx)
    SetPropMono = 87,
    /// `(obj, index_value) -> value` — dense array element read (no ctx)
    GetElemDense = 88,
    /// `(ctx) -> 0_or_bailout` — JIT back-edge tier-up check (no ctx needed for the check itself)
    CheckTierUp = 89,
}

/// Total number of helper kinds.
pub const HELPER_COUNT: usize = 90;

/// Number of helper telemetry families.
pub const HELPER_FAMILY_COUNT: usize = 10;

const HELPER_FAMILY_NAMES: [&str; HELPER_FAMILY_COUNT] = [
    "globals_scope",
    "property_element",
    "call_construct",
    "allocation_closure",
    "generic_op",
    "type_conversion",
    "iteration_spread",
    "exception_control",
    "class_super",
    "module_eval",
];

/// Primary helper safety taxonomy used by the JIT gap plan.
///
/// This is intentionally conservative: if a helper can allocate, re-enter VM
/// execution, or cross a host boundary on any path, it is classified into the
/// higher-risk bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperSafetyClass {
    /// Leaf helper operating on already-materialized values without VM/host re-entry.
    LeafNoGc,
    /// Touches VM frame/exception state but does not intentionally re-enter execution.
    VmStateOnly,
    /// Leaf helper that may allocate, intern strings, or grow object/array storage.
    AllocatingLeaf,
    /// Re-enters interpreter/JIT execution or other VM coercion/call paths.
    VmReentry,
    /// Crosses into host hooks or FFI and therefore escapes JIT-side invariants.
    HostBoundary,
    /// Never completes normally; intentionally returns `BAILOUT_SENTINEL`.
    AlwaysBailout,
}

impl HelperSafetyClass {
    /// Relative severity used when collapsing multiple helper classes into a
    /// single function-level boundary summary.
    pub const fn severity_rank(self) -> u8 {
        match self {
            Self::LeafNoGc => 0,
            Self::VmStateOnly => 1,
            Self::AllocatingLeaf => 2,
            Self::VmReentry => 3,
            Self::HostBoundary => 4,
            Self::AlwaysBailout => 5,
        }
    }

    /// Stable user-facing label for dashboards and audit docs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LeafNoGc => "leaf_no_gc",
            Self::VmStateOnly => "vm_state_only",
            Self::AllocatingLeaf => "allocating_leaf",
            Self::VmReentry => "vm_reentry",
            Self::HostBoundary => "host_boundary",
            Self::AlwaysBailout => "always_bailout",
        }
    }

    /// Whether helpers in this class may allocate or mutate heap storage.
    pub const fn may_allocate(self) -> bool {
        matches!(
            self,
            Self::AllocatingLeaf | Self::VmReentry | Self::HostBoundary
        )
    }

    /// Whether helpers in this class may re-enter interpreter/JIT execution.
    pub const fn may_reenter_vm(self) -> bool {
        matches!(self, Self::VmReentry | Self::HostBoundary)
    }

    /// Whether helpers in this class may cross host/FFI boundaries.
    pub const fn crosses_host_boundary(self) -> bool {
        matches!(self, Self::HostBoundary)
    }

    /// Whether helpers in this class can invalidate the historical no-GC assumption.
    pub const fn violates_no_gc_contract(self) -> bool {
        self.may_allocate() || self.may_reenter_vm()
    }

    /// Whether helpers in this class always force interpreter fallback.
    pub const fn always_bails_out(self) -> bool {
        matches!(self, Self::AlwaysBailout)
    }

    /// Return the more severe of two helper classes.
    pub const fn max(self, other: Self) -> Self {
        if self.severity_rank() >= other.severity_rank() {
            self
        } else {
            other
        }
    }
}

static HELPER_CALL_TOTAL: AtomicU64 = AtomicU64::new(0);
static HELPER_CALL_COUNTS: [AtomicU64; HELPER_COUNT] = [const { AtomicU64::new(0) }; HELPER_COUNT];
static HELPER_FAMILY_CALL_COUNTS: [AtomicU64; HELPER_FAMILY_COUNT] =
    [const { AtomicU64::new(0) }; HELPER_FAMILY_COUNT];
#[cfg(not(test))]
static HELPER_STATS_ENABLED: OnceLock<bool> = OnceLock::new();

/// Snapshot of JIT helper runtime telemetry.
#[derive(Debug, Clone)]
pub struct HelperCallStatsSnapshot {
    /// Total helper calls observed across all helper kinds.
    pub total_calls: u64,
    /// Per-helper call counters, indexed by [`HelperKind as usize`].
    pub per_helper: [u64; HELPER_COUNT],
    /// Per-family call counters, indexed via [`helper_family_name`].
    pub per_family: [u64; HELPER_FAMILY_COUNT],
}

#[cfg(not(test))]
fn parse_env_truthy(value: &str) -> bool {
    !matches!(value.trim(), "" | "0")
        && !value.trim().eq_ignore_ascii_case("false")
        && !value.trim().eq_ignore_ascii_case("off")
        && !value.trim().eq_ignore_ascii_case("no")
}

fn helper_stats_enabled() -> bool {
    #[cfg(test)]
    {
        true
    }

    #[cfg(not(test))]
    {
        *HELPER_STATS_ENABLED.get_or_init(|| {
            std::env::var("OTTER_JIT_STATS")
                .ok()
                .is_some_and(|value| parse_env_truthy(&value))
        })
    }
}

/// Human-readable helper family name for the provided index.
pub fn helper_family_name(index: usize) -> &'static str {
    HELPER_FAMILY_NAMES.get(index).copied().unwrap_or("unknown")
}

/// Record one runtime helper invocation.
pub fn record_helper_call(kind: HelperKind) {
    if !helper_stats_enabled() {
        return;
    }

    HELPER_CALL_TOTAL.fetch_add(1, Ordering::Relaxed);
    HELPER_CALL_COUNTS[kind as usize].fetch_add(1, Ordering::Relaxed);
    HELPER_FAMILY_CALL_COUNTS[kind.family_index()].fetch_add(1, Ordering::Relaxed);
}

/// Take a snapshot of current helper call telemetry.
pub fn helper_call_stats_snapshot() -> HelperCallStatsSnapshot {
    let mut per_helper = [0_u64; HELPER_COUNT];
    for (index, slot) in HELPER_CALL_COUNTS.iter().enumerate() {
        per_helper[index] = slot.load(Ordering::Relaxed);
    }

    let mut per_family = [0_u64; HELPER_FAMILY_COUNT];
    for (index, slot) in HELPER_FAMILY_CALL_COUNTS.iter().enumerate() {
        per_family[index] = slot.load(Ordering::Relaxed);
    }

    HelperCallStatsSnapshot {
        total_calls: HELPER_CALL_TOTAL.load(Ordering::Relaxed),
        per_helper,
        per_family,
    }
}

/// Clear helper call counters between tests.
#[doc(hidden)]
pub fn clear_helper_call_stats_for_tests() {
    HELPER_CALL_TOTAL.store(0, Ordering::Relaxed);
    for slot in &HELPER_CALL_COUNTS {
        slot.store(0, Ordering::Relaxed);
    }
    for slot in &HELPER_FAMILY_CALL_COUNTS {
        slot.store(0, Ordering::Relaxed);
    }
}

/// Byte offset of `secondary_result` field in JitContext (`#[repr(C)]`).
/// Used by IteratorNext to return both value and done flag.
/// MUST match JitContext layout in otter-vm-core/src/jit_helpers.rs.
/// Layout: function_ptr(0) proto_epoch(8) interpreter(16) vm_ctx(24)
///         constants(32) upvalues_ptr(40) upvalue_count:u32(48) pad(52)
///         this_raw(56) callee_raw(64) home_object_raw(72) secondary_result(80)
///         bailout_reason(88) bailout_pc(96)
///         deopt_locals_ptr(104) deopt_locals_count:u32(112) pad(116)
///         deopt_regs_ptr(120) deopt_regs_count:u32(128) pad(132)
///         osr_entry_pc(136)
pub const JIT_CTX_UPVALUES_PTR_OFFSET: i32 = 40;

/// Byte offset of `upvalue_count` in JitContext (`#[repr(C)]`).
pub const JIT_CTX_UPVALUE_COUNT_OFFSET: i32 = 48;

/// Size of one `UpvalueCell` entry in the upvalue slice.
pub const JIT_UPVALUE_CELL_SIZE: i32 = 8;

/// Byte offset of the raw `GcBox<UpvalueData>` pointer inside `UpvalueCell`.
pub const JIT_UPVALUE_CELL_GCBOX_PTR_OFFSET: i32 = 0;

/// Byte offset of the `value` field inside `GcBox<UpvalueData>`.
pub const JIT_UPVALUE_GCBOX_VALUE_OFFSET: i32 = 8;

/// Byte offset of the raw `Value` payload inside `UpvalueData`.
pub const JIT_UPVALUE_DATA_VALUE_OFFSET: i32 = 0;

/// Byte offset of `secondary_result` in JitContext (`#[repr(C)]`).
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

/// Byte offset of `osr_entry_pc` in JitContext (`#[repr(C)]`).
/// Layout: deopt_regs_count:u32(128) pad(132) osr_entry_pc:i64(136)
pub const JIT_CTX_OSR_ENTRY_PC_OFFSET: i32 = 136;

/// Byte offset of `tier_up_budget` in JitContext (`#[repr(C)]`).
/// Layout: osr_entry_pc:i64(136) tier_up_budget:i64(144)
pub const JIT_CTX_TIER_UP_BUDGET_OFFSET: i32 = 144;

/// Default tier-up budget for JIT back-edge recompilation checks.
/// After this many backward jumps, a tier-up check fires.
/// Low budget (100) ensures IC warmup is detected quickly. The helper call
/// (every 100 iterations) costs ~10ns — negligible vs the property access
/// savings from recompilation.
pub const JIT_TIER_UP_BUDGET_DEFAULT: i64 = 100;

// ---------------------------------------------------------------------------
// JsObject layout offsets — JsObject uses #[repr(C)], shape_tag is first field
// ---------------------------------------------------------------------------

/// Byte offset of `shape_tag: Cell<u64>` within JsObject.
/// Cell<u64> is `#[repr(transparent)]` so this is just a u64 at offset 0.
pub const JSOBJECT_SHAPE_TAG_OFFSET: i32 = 0;

/// SlotMeta KIND_DATA constant (lower 2 bits = 0b01).
pub const SLOTMETA_KIND_DATA: i64 = 0b01;

/// SlotMeta KIND_MASK (lower 2 bits).
pub const SLOTMETA_KIND_MASK: i64 = 0b11;

/// JsObject layout offsets for inline property access.
/// Computed once at startup by otter-vm-core, used by the JIT translator
/// to emit direct memory loads instead of helper function calls.
#[derive(Debug, Clone, Copy)]
pub struct JsObjectLayoutOffsets {
    /// Byte offset from JsObject start to the first Value in inline_slots data.
    /// Skips through ObjectCell → RefCell borrow flag → actual [Value; 8] data.
    pub inline_slots_data: i32,
    /// Byte offset from JsObject start to the first SlotMeta in inline_meta data.
    pub inline_meta_data: i32,
}

static JSOBJECT_LAYOUT: std::sync::OnceLock<JsObjectLayoutOffsets> = std::sync::OnceLock::new();

/// Set the JsObject layout offsets (called once at VM startup).
pub fn set_jsobject_layout(offsets: JsObjectLayoutOffsets) {
    let _ = JSOBJECT_LAYOUT.set(offsets);
}

/// Get the JsObject layout offsets (returns None if not yet initialized).
pub fn jsobject_layout() -> Option<JsObjectLayoutOffsets> {
    JSOBJECT_LAYOUT.get().copied()
}

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
            Self::CallMono => "otter_rt_call_mono",
            Self::GetElemInt => "otter_rt_get_elem_int",
            Self::CallFfi => "otter_rt_call_ffi",
            Self::SetPropMono => "otter_rt_set_prop_mono",
            Self::GetElemDense => "otter_rt_get_elem_dense",
            Self::CheckTierUp => "otter_rt_check_tier_up",
        }
    }

    /// Telemetry family index used for aggregated helper counters.
    pub const fn family_index(self) -> usize {
        match self {
            Self::LoadConst
            | Self::GetGlobal
            | Self::SetGlobal
            | Self::GetUpvalue
            | Self::SetUpvalue
            | Self::LoadThis
            | Self::CloseUpvalue
            | Self::DeclareGlobalVar
            | Self::CreateArguments => 0,

            Self::GetPropConst
            | Self::SetPropConst
            | Self::GetProp
            | Self::SetProp
            | Self::GetElem
            | Self::SetElem
            | Self::DefineProperty
            | Self::DeleteProp
            | Self::GetPropMono
            | Self::GetElemInt
            | Self::SetPropMono
            | Self::GetElemDense => 1,

            Self::CallFunction
            | Self::Construct
            | Self::CallMethod
            | Self::CallWithReceiver
            | Self::CallMethodComputed
            | Self::CallSpread
            | Self::ConstructSpread
            | Self::CallMethodComputedSpread
            | Self::TailCallHelper
            | Self::CallMono
            | Self::CallFfi => 2,

            Self::CreateClosure
            | Self::NewObject
            | Self::NewArray
            | Self::ClosureCreate
            | Self::AsyncClosure
            | Self::GeneratorClosure
            | Self::AsyncGeneratorClosure => 3,

            Self::GenericAdd
            | Self::GenericSub
            | Self::GenericMul
            | Self::GenericDiv
            | Self::GenericMod
            | Self::GenericNeg
            | Self::GenericInc
            | Self::GenericDec
            | Self::GenericLt
            | Self::GenericLe
            | Self::GenericGt
            | Self::GenericGe
            | Self::GenericEq
            | Self::GenericNeq
            | Self::GenericBitOp
            | Self::GenericBitNot
            | Self::GenericNot
            | Self::Pow => 4,

            Self::TypeOf
            | Self::TypeOfName
            | Self::ToNumber
            | Self::JsToString
            | Self::RequireCoercible
            | Self::InstanceOf
            | Self::InOp => 5,

            Self::SpreadArray
            | Self::GetIterator
            | Self::IteratorNext
            | Self::IteratorClose
            | Self::GetAsyncIterator
            | Self::ForInNext => 6,

            Self::ThrowValue
            | Self::TryStart
            | Self::TryEnd
            | Self::CatchOp
            | Self::YieldOp
            | Self::AwaitOp
            | Self::CheckTierUp => 7,

            Self::DefineGetter
            | Self::DefineSetter
            | Self::DefineMethod
            | Self::DefineClass
            | Self::GetSuper
            | Self::CallSuper
            | Self::GetSuperProp
            | Self::SetHomeObject
            | Self::CallSuperForward
            | Self::CallSuperSpread => 8,

            Self::CallEval | Self::ImportOp | Self::ExportOp => 9,
        }
    }

    /// Human-readable helper family name.
    pub fn family_name(self) -> &'static str {
        helper_family_name(self.family_index())
    }

    /// Conservative safety classification used for helper audits and future
    /// tiering/safepoint policy.
    pub const fn safety_class(self) -> HelperSafetyClass {
        match self {
            Self::LoadThis
            | Self::GetUpvalue
            | Self::SetUpvalue
            | Self::Pow
            | Self::GetElemInt
            | Self::GetPropMono
            | Self::GenericSub
            | Self::GenericMul
            | Self::GenericDiv
            | Self::GenericMod
            | Self::GenericNeg
            | Self::GenericInc
            | Self::GenericDec
            | Self::GenericLt
            | Self::GenericLe
            | Self::GenericGt
            | Self::GenericGe
            | Self::GenericEq
            | Self::GenericNeq
            | Self::GenericBitOp
            | Self::GenericBitNot
            | Self::GenericNot
            | Self::RequireCoercible
            | Self::SetPropMono
            | Self::GetElemDense
            | Self::CheckTierUp => HelperSafetyClass::LeafNoGc,

            Self::CloseUpvalue | Self::TryStart | Self::TryEnd | Self::CatchOp => {
                HelperSafetyClass::VmStateOnly
            }

            Self::LoadConst
            | Self::GetGlobal
            | Self::SetGlobal
            | Self::GetPropConst
            | Self::SetPropConst
            | Self::GetProp
            | Self::SetProp
            | Self::NewObject
            | Self::NewArray
            | Self::GetElem
            | Self::SetElem
            | Self::DefineProperty
            | Self::DeleteProp
            | Self::GenericAdd
            | Self::TypeOf
            | Self::TypeOfName
            | Self::InstanceOf
            | Self::InOp
            | Self::DeclareGlobalVar
            | Self::DefineGetter
            | Self::DefineSetter
            | Self::DefineMethod
            | Self::SpreadArray
            | Self::CreateClosure
            | Self::ClosureCreate
            | Self::DefineClass
            | Self::GetSuper
            | Self::GetSuperProp
            | Self::SetHomeObject
            | Self::AsyncClosure
            | Self::GeneratorClosure
            | Self::AsyncGeneratorClosure => HelperSafetyClass::AllocatingLeaf,

            Self::CallFunction
            | Self::Construct
            | Self::CallMethod
            | Self::CallWithReceiver
            | Self::CallMethodComputed
            | Self::ToNumber
            | Self::JsToString
            | Self::GetIterator
            | Self::IteratorNext
            | Self::IteratorClose
            | Self::CallSpread
            | Self::ConstructSpread
            | Self::CallMethodComputedSpread
            | Self::TailCallHelper
            | Self::CallSuper
            | Self::CallSuperForward
            | Self::CallSuperSpread
            | Self::GetAsyncIterator
            | Self::CallMono
            | Self::CallEval => HelperSafetyClass::VmReentry,

            Self::ImportOp | Self::ExportOp | Self::ForInNext | Self::CallFfi => {
                HelperSafetyClass::HostBoundary
            }

            Self::ThrowValue | Self::CreateArguments | Self::YieldOp | Self::AwaitOp => {
                HelperSafetyClass::AlwaysBailout
            }
        }
    }

    /// Whether this helper can allocate or otherwise force a GC-relevant heap mutation.
    pub const fn may_allocate(self) -> bool {
        self.safety_class().may_allocate()
    }

    /// Whether this helper may re-enter interpreter/JIT execution.
    pub const fn may_reenter_vm(self) -> bool {
        self.safety_class().may_reenter_vm()
    }

    /// Whether this helper crosses host hooks or FFI.
    pub const fn crosses_host_boundary(self) -> bool {
        self.safety_class().crosses_host_boundary()
    }

    /// Whether this helper violates the historical "no GC during JIT" assumption.
    pub const fn violates_no_gc_contract(self) -> bool {
        self.safety_class().violates_no_gc_contract()
    }

    /// Whether this helper intentionally always bails out.
    pub const fn always_bails_out(self) -> bool {
        self.safety_class().always_bails_out()
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
            | Self::AwaitOp
            | Self::CheckTierUp => 1,
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
            | Self::GetPropMono
            | Self::GetElemInt
            | Self::GetElemDense => 3,
            Self::GetPropConst
            | Self::GetProp
            | Self::GetElem
            | Self::CallFunction
            | Self::Construct
            | Self::DefineProperty
            | Self::DefineGetter
            | Self::DefineSetter
            | Self::DefineMethod
            | Self::GenericBitOp
            | Self::InstanceOf
            | Self::InOp
            | Self::TailCallHelper
            | Self::DefineClass
            | Self::SetPropMono => 4,
            Self::SetPropConst
            | Self::SetProp
            | Self::SetElem
            | Self::SetGlobal
            | Self::CallWithReceiver
            | Self::CallSpread
            | Self::ConstructSpread
            | Self::CallMethodComputedSpread
            | Self::CallMono
            | Self::CallFfi => 5,
            Self::CallMethod | Self::CallMethodComputed => 6,
            Self::ImportOp | Self::ForInNext => 2,
            Self::ExportOp => 3,
        }
    }

    /// Number of return values (always 1 — i64).
    pub fn return_count(self) -> usize {
        1
    }

    /// Build the Cranelift IR signature for this helper using the given calling convention.
    pub fn make_signature_with_call_conv(
        self,
        call_conv: cranelift_codegen::isa::CallConv,
    ) -> ir::Signature {
        let mut sig = ir::Signature::new(call_conv);
        for _ in 0..self.param_count() {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..self.return_count() {
            sig.returns.push(AbiParam::new(types::I64));
        }
        sig
    }

    /// Build the Cranelift IR signature for this helper (SystemV fallback).
    pub fn make_signature(self) -> ir::Signature {
        self.make_signature_with_call_conv(cranelift_codegen::isa::CallConv::SystemV)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_call_snapshot_tracks_totals_and_families() {
        clear_helper_call_stats_for_tests();

        record_helper_call(HelperKind::GetPropConst);
        record_helper_call(HelperKind::CallFunction);
        record_helper_call(HelperKind::CallFunction);

        let snapshot = helper_call_stats_snapshot();
        assert_eq!(snapshot.total_calls, 3);
        assert_eq!(snapshot.per_helper[HelperKind::GetPropConst as usize], 1);
        assert_eq!(snapshot.per_helper[HelperKind::CallFunction as usize], 2);
        assert_eq!(
            snapshot.per_family[HelperKind::GetPropConst.family_index()],
            1
        );
        assert_eq!(
            snapshot.per_family[HelperKind::CallFunction.family_index()],
            2
        );
    }

    #[test]
    fn helper_safety_classes_mark_gc_and_reentry_boundaries() {
        assert_eq!(
            HelperKind::GetPropMono.safety_class(),
            HelperSafetyClass::LeafNoGc
        );
        assert!(!HelperKind::GetPropMono.violates_no_gc_contract());

        assert_eq!(
            HelperKind::CloseUpvalue.safety_class(),
            HelperSafetyClass::VmStateOnly
        );
        assert!(!HelperKind::CloseUpvalue.violates_no_gc_contract());

        assert_eq!(
            HelperKind::NewObject.safety_class(),
            HelperSafetyClass::AllocatingLeaf
        );
        assert!(HelperKind::NewObject.may_allocate());
        assert!(HelperKind::NewObject.violates_no_gc_contract());
        assert_eq!(
            HelperKind::ClosureCreate.safety_class(),
            HelperSafetyClass::AllocatingLeaf
        );
        assert!(HelperKind::ClosureCreate.may_allocate());
        assert!(!HelperKind::ClosureCreate.always_bails_out());

        assert_eq!(
            HelperKind::CallFunction.safety_class(),
            HelperSafetyClass::VmReentry
        );
        assert!(HelperKind::CallFunction.may_reenter_vm());
        assert!(HelperKind::CallFunction.violates_no_gc_contract());

        assert_eq!(
            HelperKind::ImportOp.safety_class(),
            HelperSafetyClass::HostBoundary
        );
        assert!(HelperKind::ImportOp.crosses_host_boundary());
        assert!(HelperKind::ImportOp.violates_no_gc_contract());

        assert_eq!(
            HelperKind::YieldOp.safety_class(),
            HelperSafetyClass::AlwaysBailout
        );
        assert!(HelperKind::YieldOp.always_bails_out());
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
pub struct HelperFuncIds {
    ids: [Option<FuncId>; HELPER_COUNT],
}

impl HelperFuncIds {
    /// Declare all available helpers as imported functions on the module.
    #[allow(dead_code)]
    pub fn declare<M: Module>(helpers: &RuntimeHelpers, module: &mut M) -> Result<Self, JitError> {
        Self::declare_with_call_conv(helpers, module, cranelift_codegen::isa::CallConv::SystemV)
    }

    /// Declare all available helpers with a specific calling convention.
    pub fn declare_with_call_conv<M: Module>(
        helpers: &RuntimeHelpers,
        module: &mut M,
        call_conv: cranelift_codegen::isa::CallConv,
    ) -> Result<Self, JitError> {
        let mut ids = [None; HELPER_COUNT];
        for (i, ptr) in helpers.ptrs.iter().enumerate() {
            if ptr.is_some() {
                let kind = unsafe { std::mem::transmute::<u8, HelperKind>(i as u8) };
                let sig = kind.make_signature_with_call_conv(call_conv);
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
pub struct HelperRefs {
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
    #[allow(dead_code)]
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
