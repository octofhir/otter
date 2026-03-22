//! MIR operation definitions.
//!
//! Each `MirOp` represents a single operation in the MIR graph.
//! Operations are SSA: each produces at most one value (`ValueId`),
//! and operands are references to previously-produced values.

use super::graph::{BlockId, DeoptId, ValueId};
use super::types::{CmpOp, MirType};

/// A MIR operation. Each instruction in a basic block is one `MirOp`.
#[derive(Debug, Clone)]
pub enum MirOp {
    // ====================================================================
    // Constants
    // ====================================================================
    /// Load a NaN-boxed constant (u64 bit pattern).
    Const(u64),
    /// Undefined singleton.
    Undefined,
    /// Null singleton.
    Null,
    /// Boolean true.
    True,
    /// Boolean false.
    False,
    /// Int32 constant (will be boxed or used unboxed depending on context).
    ConstInt32(i32),
    /// Float64 constant.
    ConstFloat64(f64),

    // ====================================================================
    // Guards (deopt on failure)
    // ====================================================================
    /// Assert value is Int32, unbox it. Deopt if not.
    GuardInt32 { val: ValueId, deopt: DeoptId },
    /// Assert value is Float64, unbox it. Deopt if not.
    GuardFloat64 { val: ValueId, deopt: DeoptId },
    /// Assert value is an object pointer, extract it. Deopt if not.
    GuardObject { val: ValueId, deopt: DeoptId },
    /// Assert value is a string pointer, extract it. Deopt if not.
    GuardString { val: ValueId, deopt: DeoptId },
    /// Assert value is a function pointer, extract it. Deopt if not.
    GuardFunction { val: ValueId, deopt: DeoptId },
    /// Assert value is boolean, extract it. Deopt if not.
    GuardBool { val: ValueId, deopt: DeoptId },
    /// Assert object has the expected shape. Deopt if not.
    GuardShape {
        obj: ValueId,
        shape_id: u64,
        deopt: DeoptId,
    },
    /// Assert prototype epoch hasn't changed. Deopt if not.
    GuardProtoEpoch { epoch: u64, deopt: DeoptId },
    /// Assert object is a dense array. Deopt if not.
    GuardArrayDense { obj: ValueId, deopt: DeoptId },
    /// Assert array index is in bounds. Deopt if not.
    GuardBoundsCheck {
        arr: ValueId,
        idx: ValueId,
        deopt: DeoptId,
    },
    /// Assert value is not a hole (sparse array sentinel). Deopt if not.
    GuardNotHole { val: ValueId, deopt: DeoptId },

    // ====================================================================
    // Boxing / Unboxing
    // ====================================================================
    /// Box an i32 into a NaN-boxed value.
    BoxInt32(ValueId),
    /// Box an f64 into a NaN-boxed value.
    BoxFloat64(ValueId),
    /// Box a bool into a NaN-boxed value.
    BoxBool(ValueId),
    /// Unchecked unbox Int32 (caller must have guarded).
    UnboxInt32(ValueId),
    /// Unchecked unbox Float64.
    UnboxFloat64(ValueId),
    /// Convert Int32 to Float64.
    Int32ToFloat64(ValueId),

    // ====================================================================
    // Typed Arithmetic (overflow → deopt)
    // ====================================================================
    /// i32 addition with overflow check.
    AddI32 {
        lhs: ValueId,
        rhs: ValueId,
        deopt: DeoptId,
    },
    /// i32 subtraction with overflow check.
    SubI32 {
        lhs: ValueId,
        rhs: ValueId,
        deopt: DeoptId,
    },
    /// i32 multiplication with overflow check.
    MulI32 {
        lhs: ValueId,
        rhs: ValueId,
        deopt: DeoptId,
    },
    /// i32 division. Deopt on div-by-zero or non-integer result.
    DivI32 {
        lhs: ValueId,
        rhs: ValueId,
        deopt: DeoptId,
    },
    /// i32 increment (add 1) with overflow check.
    IncI32 { val: ValueId, deopt: DeoptId },
    /// i32 decrement (sub 1) with overflow check.
    DecI32 { val: ValueId, deopt: DeoptId },
    /// i32 negation with overflow check (INT_MIN).
    NegI32 { val: ValueId, deopt: DeoptId },

    /// f64 addition.
    AddF64 { lhs: ValueId, rhs: ValueId },
    /// f64 subtraction.
    SubF64 { lhs: ValueId, rhs: ValueId },
    /// f64 multiplication.
    MulF64 { lhs: ValueId, rhs: ValueId },
    /// f64 division.
    DivF64 { lhs: ValueId, rhs: ValueId },
    /// f64 modulo.
    ModF64 { lhs: ValueId, rhs: ValueId },
    /// f64 negation.
    NegF64(ValueId),

    // ====================================================================
    // Bitwise Operations (always i32)
    // ====================================================================
    BitAnd { lhs: ValueId, rhs: ValueId },
    BitOr { lhs: ValueId, rhs: ValueId },
    BitXor { lhs: ValueId, rhs: ValueId },
    Shl { lhs: ValueId, rhs: ValueId },
    Shr { lhs: ValueId, rhs: ValueId },
    Ushr { lhs: ValueId, rhs: ValueId },
    BitNot(ValueId),

    // ====================================================================
    // Typed Comparisons
    // ====================================================================
    /// i32 comparison.
    CmpI32 {
        op: CmpOp,
        lhs: ValueId,
        rhs: ValueId,
    },
    /// f64 comparison.
    CmpF64 {
        op: CmpOp,
        lhs: ValueId,
        rhs: ValueId,
    },
    /// Boxed strict equality (`===`).
    CmpStrictEq { lhs: ValueId, rhs: ValueId },
    /// Boxed strict inequality (`!==`).
    CmpStrictNe { lhs: ValueId, rhs: ValueId },
    /// Logical NOT on boolean.
    LogicalNot(ValueId),

    // ====================================================================
    // Property Access
    // ====================================================================
    /// Shape-guarded property load (after GuardShape).
    /// `inline` = true if offset < INLINE_PROPERTY_COUNT (8).
    GetPropShaped {
        obj: ValueId,
        offset: u32,
        inline: bool,
    },
    /// Shape-guarded property store (after GuardShape).
    SetPropShaped {
        obj: ValueId,
        offset: u32,
        val: ValueId,
        inline: bool,
    },
    /// Generic property get (cold helper call).
    GetPropGeneric {
        obj: ValueId,
        key: ValueId,
        ic_index: u16,
    },
    /// Generic property set (cold helper call).
    SetPropGeneric {
        obj: ValueId,
        key: ValueId,
        val: ValueId,
        ic_index: u16,
    },
    /// Property get by constant name (cold helper call).
    GetPropConstGeneric {
        obj: ValueId,
        name_idx: u32,
        ic_index: u16,
    },
    /// Property set by constant name (cold helper call).
    SetPropConstGeneric {
        obj: ValueId,
        name_idx: u32,
        val: ValueId,
        ic_index: u16,
    },
    /// Delete property.
    DeleteProp { obj: ValueId, key: ValueId },

    // ====================================================================
    // Array Access
    // ====================================================================
    /// Dense array element load (after GuardArrayDense + GuardBoundsCheck).
    GetElemDense { arr: ValueId, idx: ValueId },
    /// Dense array element store (after GuardArrayDense + GuardBoundsCheck).
    SetElemDense {
        arr: ValueId,
        idx: ValueId,
        val: ValueId,
    },
    /// Array length (after GuardArrayDense).
    ArrayLength(ValueId),
    /// Array push (after GuardArrayDense). Returns new length as Int32.
    ArrayPush { arr: ValueId, val: ValueId },
    /// Generic element access (cold helper call).
    GetElemGeneric {
        obj: ValueId,
        key: ValueId,
        ic_index: u16,
    },
    /// Generic element set (cold helper call).
    SetElemGeneric {
        obj: ValueId,
        key: ValueId,
        val: ValueId,
        ic_index: u16,
    },

    // ====================================================================
    // Calls
    // ====================================================================
    /// Direct call to a known function (JIT-to-JIT or JIT-to-interpreter).
    CallDirect {
        target: ValueId,
        args: Vec<ValueId>,
    },
    /// Monomorphic call (guard on call target, deopt on miss).
    CallMonomorphic {
        callee: ValueId,
        expected_bits: u64,
        args: Vec<ValueId>,
        deopt: DeoptId,
    },
    /// Generic call (cold helper).
    CallGeneric {
        callee: ValueId,
        args: Vec<ValueId>,
        ic_index: u16,
    },
    /// Method call by constant name (cold helper).
    CallMethodGeneric {
        obj: ValueId,
        name_idx: u32,
        args: Vec<ValueId>,
        ic_index: u16,
    },
    /// Generic construct (cold helper).
    ConstructGeneric {
        callee: ValueId,
        args: Vec<ValueId>,
    },

    // ====================================================================
    // Variables
    // ====================================================================
    /// Load a local variable (from register window).
    LoadLocal(u16),
    /// Store a local variable.
    StoreLocal { idx: u16, val: ValueId },
    /// Load a scratch register value.
    LoadRegister(u16),
    /// Store to a scratch register.
    StoreRegister { idx: u16, val: ValueId },
    /// Load an upvalue (closure capture).
    LoadUpvalue(u16),
    /// Store an upvalue.
    StoreUpvalue { idx: u16, val: ValueId },
    /// Close an upvalue (convert from open to closed).
    CloseUpvalue(u16),
    /// Load the `this` value.
    LoadThis,

    // ====================================================================
    // Globals
    // ====================================================================
    /// Get a global variable by name constant index.
    GetGlobal { name_idx: u32, ic_index: u16 },
    /// Set a global variable.
    SetGlobal {
        name_idx: u32,
        val: ValueId,
        ic_index: u16,
    },

    // ====================================================================
    // Object/Array Creation
    // ====================================================================
    /// Allocate a new empty object.
    NewObject,
    /// Allocate a new array with given length hint.
    NewArray { len: u16 },
    /// Create a closure from a function index.
    CreateClosure { func_idx: u32 },
    /// Create arguments object.
    CreateArguments,
    /// Define a property on an object.
    DefineProperty {
        obj: ValueId,
        key: ValueId,
        val: ValueId,
    },
    /// Set prototype of object.
    SetPrototype { obj: ValueId, proto: ValueId },

    // ====================================================================
    // Type Operations
    // ====================================================================
    /// `typeof` operator. Returns a Boxed string value.
    TypeOf(ValueId),
    /// `instanceof` operator.
    InstanceOf {
        lhs: ValueId,
        rhs: ValueId,
        ic_index: u16,
    },
    /// `in` operator.
    In {
        key: ValueId,
        obj: ValueId,
        ic_index: u16,
    },
    /// ToNumber coercion.
    ToNumber(ValueId),
    /// ToString coercion.
    ToStringOp(ValueId),
    /// RequireObjectCoercible — throw if null/undefined.
    RequireCoercible(ValueId),
    /// Is value truthy? Returns Bool.
    IsTruthy(ValueId),

    // ====================================================================
    // Control Flow
    // ====================================================================
    /// Unconditional jump to block.
    Jump(BlockId),
    /// Conditional branch.
    Branch {
        cond: ValueId,
        true_block: BlockId,
        false_block: BlockId,
    },
    /// Return a value from the function.
    Return(ValueId),
    /// Return undefined.
    ReturnUndefined,
    /// Unconditional deopt to interpreter.
    Deopt(DeoptId),

    // ====================================================================
    // Exception Handling
    // ====================================================================
    /// Push a try handler (cold helper).
    TryStart { catch_block: BlockId },
    /// Pop the current try handler.
    TryEnd,
    /// Throw a value.
    Throw(ValueId),
    /// Catch — receive the thrown exception value.
    Catch,

    // ====================================================================
    // Iteration
    // ====================================================================
    /// GetIterator (helper call).
    GetIterator(ValueId),
    /// IteratorNext — returns (value, done) pair via secondary result.
    IteratorNext(ValueId),
    /// IteratorClose.
    IteratorClose(ValueId),

    // ====================================================================
    // GC
    // ====================================================================
    /// GC safepoint. Lists all live values that may contain GC references.
    Safepoint { live: Vec<ValueId> },
    /// Write barrier for heap stores (after SetPropShaped, SetElemDense, etc.).
    WriteBarrier(ValueId),

    // ====================================================================
    // Phi
    // ====================================================================
    /// SSA phi node. Merges values from predecessor blocks.
    Phi(Vec<(BlockId, ValueId)>),

    // ====================================================================
    // Misc
    // ====================================================================
    /// Move / copy a value (SSA identity, usually eliminated).
    Move(ValueId),
    /// Load a constant from the constant pool (by index).
    LoadConstPool(u32),
    /// Spread array elements.
    Spread(ValueId),
    /// Generic cold helper call (fallback for unsupported operations).
    HelperCall {
        kind: HelperKind,
        args: Vec<ValueId>,
    },
}

impl MirOp {
    /// The result type of this operation.
    pub fn result_type(&self) -> MirType {
        match self {
            // Constants
            MirOp::Const(_) | MirOp::Undefined | MirOp::Null | MirOp::True | MirOp::False => {
                MirType::Boxed
            }
            MirOp::ConstInt32(_) => MirType::Int32,
            MirOp::ConstFloat64(_) => MirType::Float64,

            // Guards produce their specialized type
            MirOp::GuardInt32 { .. } | MirOp::UnboxInt32(_) => MirType::Int32,
            MirOp::GuardFloat64 { .. } | MirOp::UnboxFloat64(_) => MirType::Float64,
            MirOp::GuardObject { .. } => MirType::ObjectRef,
            MirOp::GuardString { .. } => MirType::StringRef,
            MirOp::GuardFunction { .. } => MirType::FunctionRef,
            MirOp::GuardBool { .. } => MirType::Bool,
            MirOp::GuardArrayDense { .. } => MirType::ArrayRef,

            // Shape/epoch/bounds guards produce Void (side-effect only)
            MirOp::GuardShape { .. }
            | MirOp::GuardProtoEpoch { .. }
            | MirOp::GuardBoundsCheck { .. }
            | MirOp::GuardNotHole { .. } => MirType::Void,

            // Boxing
            MirOp::BoxInt32(_) | MirOp::BoxFloat64(_) | MirOp::BoxBool(_) => MirType::Boxed,

            // Int32 arithmetic
            MirOp::AddI32 { .. }
            | MirOp::SubI32 { .. }
            | MirOp::MulI32 { .. }
            | MirOp::DivI32 { .. }
            | MirOp::IncI32 { .. }
            | MirOp::DecI32 { .. }
            | MirOp::NegI32 { .. } => MirType::Int32,

            // Float64 arithmetic
            MirOp::AddF64 { .. }
            | MirOp::SubF64 { .. }
            | MirOp::MulF64 { .. }
            | MirOp::DivF64 { .. }
            | MirOp::ModF64 { .. }
            | MirOp::NegF64(_) => MirType::Float64,
            MirOp::Int32ToFloat64(_) => MirType::Float64,

            // Bitwise (always i32)
            MirOp::BitAnd { .. }
            | MirOp::BitOr { .. }
            | MirOp::BitXor { .. }
            | MirOp::Shl { .. }
            | MirOp::Shr { .. }
            | MirOp::Ushr { .. }
            | MirOp::BitNot(_) => MirType::Int32,

            // Comparisons produce Bool
            MirOp::CmpI32 { .. }
            | MirOp::CmpF64 { .. }
            | MirOp::CmpStrictEq { .. }
            | MirOp::CmpStrictNe { .. }
            | MirOp::LogicalNot(_)
            | MirOp::IsTruthy(_) => MirType::Bool,

            // Property/element access produces Boxed
            MirOp::GetPropShaped { .. }
            | MirOp::GetPropGeneric { .. }
            | MirOp::GetPropConstGeneric { .. }
            | MirOp::GetElemDense { .. }
            | MirOp::GetElemGeneric { .. }
            | MirOp::DeleteProp { .. } => MirType::Boxed,

            // Property/element stores produce Void
            MirOp::SetPropShaped { .. }
            | MirOp::SetPropGeneric { .. }
            | MirOp::SetPropConstGeneric { .. }
            | MirOp::SetElemDense { .. }
            | MirOp::SetElemGeneric { .. }
            | MirOp::DefineProperty { .. }
            | MirOp::SetPrototype { .. } => MirType::Void,

            // Array
            MirOp::ArrayLength(_) => MirType::Int32,
            MirOp::ArrayPush { .. } => MirType::Int32,

            // Calls produce Boxed
            MirOp::CallDirect { .. }
            | MirOp::CallMonomorphic { .. }
            | MirOp::CallGeneric { .. }
            | MirOp::CallMethodGeneric { .. }
            | MirOp::ConstructGeneric { .. } => MirType::Boxed,

            // Variables produce Boxed
            MirOp::LoadLocal(_)
            | MirOp::LoadRegister(_)
            | MirOp::LoadUpvalue(_)
            | MirOp::LoadThis
            | MirOp::LoadConstPool(_) => MirType::Boxed,

            // Variable stores produce Void
            MirOp::StoreLocal { .. }
            | MirOp::StoreRegister { .. }
            | MirOp::StoreUpvalue { .. }
            | MirOp::CloseUpvalue(_) => MirType::Void,

            // Globals
            MirOp::GetGlobal { .. } => MirType::Boxed,
            MirOp::SetGlobal { .. } => MirType::Void,

            // Object/Array creation
            MirOp::NewObject | MirOp::NewArray { .. } | MirOp::CreateArguments => MirType::Boxed,
            MirOp::CreateClosure { .. } => MirType::Boxed,

            // Type operations
            MirOp::TypeOf(_) => MirType::Boxed,
            MirOp::InstanceOf { .. } | MirOp::In { .. } => MirType::Bool,
            MirOp::ToNumber(_) => MirType::Boxed,
            MirOp::ToStringOp(_) => MirType::Boxed,
            MirOp::RequireCoercible(_) => MirType::Void,

            // Control flow produces Void (terminators)
            MirOp::Jump(_)
            | MirOp::Branch { .. }
            | MirOp::Return(_)
            | MirOp::ReturnUndefined
            | MirOp::Deopt(_) => MirType::Void,

            // Exception handling
            MirOp::TryStart { .. } | MirOp::TryEnd | MirOp::Throw(_) => MirType::Void,
            MirOp::Catch => MirType::Boxed,

            // Iteration
            MirOp::GetIterator(_) => MirType::Boxed,
            MirOp::IteratorNext(_) => MirType::Boxed,
            MirOp::IteratorClose(_) => MirType::Void,

            // GC
            MirOp::Safepoint { .. } | MirOp::WriteBarrier(_) => MirType::Void,

            // Phi has the type of its inputs (determined by verifier).
            MirOp::Phi(_) => MirType::Boxed,

            // Misc
            MirOp::Move(v) => {
                // Move inherits source type — but we can't know it statically here.
                // The verifier resolves this. Default to Boxed.
                let _ = v;
                MirType::Boxed
            }
            MirOp::Spread(_) => MirType::Boxed,
            MirOp::HelperCall { .. } => MirType::Boxed,
        }
    }

    /// Whether this operation is a block terminator.
    pub fn is_terminator(&self) -> bool {
        matches!(
            self,
            MirOp::Jump(_)
                | MirOp::Branch { .. }
                | MirOp::Return(_)
                | MirOp::ReturnUndefined
                | MirOp::Deopt(_)
                | MirOp::Throw(_)
        )
    }

    /// Whether this operation may trigger a GC (allocation, helper call, etc.).
    pub fn may_gc(&self) -> bool {
        matches!(
            self,
            MirOp::NewObject
                | MirOp::NewArray { .. }
                | MirOp::CreateClosure { .. }
                | MirOp::CreateArguments
                | MirOp::CallDirect { .. }
                | MirOp::CallMonomorphic { .. }
                | MirOp::CallGeneric { .. }
                | MirOp::CallMethodGeneric { .. }
                | MirOp::ConstructGeneric { .. }
                | MirOp::GetPropGeneric { .. }
                | MirOp::SetPropGeneric { .. }
                | MirOp::GetPropConstGeneric { .. }
                | MirOp::SetPropConstGeneric { .. }
                | MirOp::GetElemGeneric { .. }
                | MirOp::SetElemGeneric { .. }
                | MirOp::ToStringOp(_)
                | MirOp::ToNumber(_)
                | MirOp::GetIterator(_)
                | MirOp::IteratorNext(_)
                | MirOp::IteratorClose(_)
                | MirOp::ArrayPush { .. }
                | MirOp::Spread(_)
                | MirOp::HelperCall { .. }
        )
    }
}

/// Identifies a runtime helper kind for cold-exit `HelperCall`.
/// This is a simplified version — full list will grow as we add support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HelperKind {
    GenericAdd,
    GenericSub,
    GenericMul,
    GenericDiv,
    GenericMod,
    GenericNeg,
    GenericInc,
    GenericDec,
    GenericEq,
    GenericStrictEq,
    GenericLt,
    GenericLe,
    GenericGt,
    GenericGe,
    Pow,
    ForInNext,
    CallEval,
    ThrowIfNotObject,
    DeclareGlobalVar,
    DefineGetter,
    DefineSetter,
    DefineMethod,
}
