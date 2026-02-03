//! Bytecode instructions (opcodes)

use serde::{Deserialize, Serialize};

use crate::operand::{ConstantIndex, FunctionIndex, JumpOffset, LocalIndex, Register};

/// Bytecode opcodes
///
/// Register-based instruction set. Most instructions take a destination register
/// and one or more source registers/operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Opcode {
    // ==================== Constants ====================
    /// Load undefined into register: dst = undefined
    LoadUndefined = 0x00,
    /// Load null into register: dst = null
    LoadNull = 0x01,
    /// Load true into register: dst = true
    LoadTrue = 0x02,
    /// Load false into register: dst = false
    LoadFalse = 0x03,
    /// Load integer immediate (-128..127): dst = imm8
    LoadInt8 = 0x04,
    /// Load integer immediate (32-bit): dst = imm32
    LoadInt32 = 0x05,
    /// Load constant from pool: dst = constants\[idx\]
    LoadConst = 0x06,

    // ==================== Variables ====================
    /// Load local variable: dst = locals\[idx\]
    GetLocal = 0x10,
    /// Store to local variable: locals\[idx\] = src
    SetLocal = 0x11,
    /// Load from closure (upvalue): dst = upvalues\[idx\]
    GetUpvalue = 0x12,
    /// Store to closure (upvalue): upvalues\[idx\] = src
    SetUpvalue = 0x13,
    /// Load global variable: dst = globals\[name\]
    GetGlobal = 0x14,
    /// Store global variable: globals\[name\] = src
    SetGlobal = 0x15,
    /// Load `this` value: dst = this
    LoadThis = 0x16,
    /// Close upvalue: move local to heap cell when leaving scope
    CloseUpvalue = 0x17,

    // ==================== Arithmetic ====================
    /// Addition: dst = lhs + rhs
    Add = 0x20,
    /// Subtraction: dst = lhs - rhs
    Sub = 0x21,
    /// Multiplication: dst = lhs * rhs
    Mul = 0x22,
    /// Division: dst = lhs / rhs
    Div = 0x23,
    /// Modulo: dst = lhs % rhs
    Mod = 0x24,
    /// Exponentiation: dst = lhs ** rhs
    Pow = 0x25,
    /// Unary negation: dst = -src
    Neg = 0x26,
    /// Increment: dst = src + 1
    Inc = 0x27,
    /// Decrement: dst = src - 1
    Dec = 0x28,

    // ==================== Bitwise ====================
    /// Bitwise AND: dst = lhs & rhs
    BitAnd = 0x30,
    /// Bitwise OR: dst = lhs | rhs
    BitOr = 0x31,
    /// Bitwise XOR: dst = lhs ^ rhs
    BitXor = 0x32,
    /// Bitwise NOT: dst = ~src
    BitNot = 0x33,
    /// Left shift: dst = lhs << rhs
    Shl = 0x34,
    /// Signed right shift: dst = lhs >> rhs
    Shr = 0x35,
    /// Unsigned right shift: dst = lhs >>> rhs
    Ushr = 0x36,

    // ==================== Comparison ====================
    /// Equality: dst = lhs == rhs
    Eq = 0x40,
    /// Strict equality: dst = lhs === rhs
    StrictEq = 0x41,
    /// Inequality: dst = lhs != rhs
    Ne = 0x42,
    /// Strict inequality: dst = lhs !== rhs
    StrictNe = 0x43,
    /// Less than: dst = lhs < rhs
    Lt = 0x44,
    /// Less than or equal: dst = lhs <= rhs
    Le = 0x45,
    /// Greater than: dst = lhs > rhs
    Gt = 0x46,
    /// Greater than or equal: dst = lhs >= rhs
    Ge = 0x47,

    // ==================== Logical ====================
    /// Logical NOT: dst = !src
    Not = 0x50,

    // ==================== Type Operations ====================
    /// typeof operator: dst = typeof src
    TypeOf = 0x58,
    /// instanceof operator: dst = lhs instanceof rhs
    InstanceOf = 0x59,
    /// in operator: dst = lhs in rhs
    In = 0x5A,
    /// ToNumber conversion: dst = +src
    ToNumber = 0x5B,
    /// typeof identifier name without ReferenceError
    TypeOfName = 0x5C,
    /// RequireObjectCoercible - throws TypeError if value is null or undefined
    RequireCoercible = 0x5D,

    // ==================== Objects ====================
    /// Get property: dst = obj\[key\]
    GetProp = 0x60,
    /// Set property: obj\[key\] = val
    SetProp = 0x61,
    /// Get property by constant name: dst = obj.name
    GetPropConst = 0x62,
    /// Set property by constant name: obj.name = val
    SetPropConst = 0x63,
    /// Delete property: dst = delete obj\[key\]
    DeleteProp = 0x64,
    /// Create empty object: dst = {}
    NewObject = 0x65,
    /// Define property on object
    DefineProperty = 0x66,
    /// Define getter on object: Object.defineProperty(obj, key, {get: fn})
    DefineGetter = 0x67,
    /// Define setter on object: Object.defineProperty(obj, key, {set: fn})
    DefineSetter = 0x68,

    // ==================== Arrays ====================
    /// Create empty array: dst = \[\]
    NewArray = 0x70,
    /// Get element: dst = arr\[idx\]
    GetElem = 0x71,
    /// Set element: arr\[idx\] = val
    SetElem = 0x72,
    /// Array spread: ...arr
    Spread = 0x73,

    // ==================== Functions ====================
    /// Create closure: dst = closure(func_idx)
    Closure = 0x80,
    /// Call function: dst = func(args...)
    Call = 0x81,
    /// Call method: dst = obj.method(args...)
    CallMethod = 0x82,
    /// Tail call optimization: return func(args...)
    TailCall = 0x83,
    /// Construct object: dst = new func(args...)
    Construct = 0x84,
    /// Return value from function
    Return = 0x85,
    /// Return undefined from function
    ReturnUndefined = 0x86,
    /// Call with spread arguments: dst = func(...spread_arr)
    CallSpread = 0x87,
    /// Construct with spread arguments: dst = new func(...spread_arr)
    ConstructSpread = 0x88,
    /// Call function with explicit receiver: dst = func.call(this, args...)
    CallWithReceiver = 0x89,
    /// Create arguments object: dst = arguments
    CreateArguments = 0x8A,
    /// Call eval: dst = eval(code)
    CallEval = 0x8B,

    // ==================== Control Flow ====================
    /// Unconditional jump
    Jump = 0x90,
    /// Jump if true
    JumpIfTrue = 0x91,
    /// Jump if false
    JumpIfFalse = 0x92,
    /// Jump if null or undefined
    JumpIfNullish = 0x93,
    /// Jump if not null and not undefined
    JumpIfNotNullish = 0x94,

    // ==================== Exception Handling ====================
    /// Begin try block
    TryStart = 0xA0,
    /// End try block
    TryEnd = 0xA1,
    /// Throw exception
    Throw = 0xA2,
    /// Catch exception into register
    Catch = 0xA3,

    // ==================== Iteration ====================
    /// Get iterator: dst = obj[Symbol.iterator]()
    GetIterator = 0xB0,
    /// Iterator next: dst = iter.next()
    IteratorNext = 0xB1,
    /// Iterate over iterable (for-of): jump if done
    ForInNext = 0xB2,
    /// Get async iterator: dst = obj[Symbol.asyncIterator]() or fallback to Symbol.iterator
    GetAsyncIterator = 0xB3,

    // ==================== Class ====================
    /// Define class
    DefineClass = 0xC0,
    /// Get super: dst = super
    GetSuper = 0xC1,
    /// Call super constructor
    CallSuper = 0xC2,
    /// Get super property: dst = super\[key\]
    GetSuperProp = 0xC3,
    /// Set [[HomeObject]] on a closure for super resolution
    SetHomeObject = 0xC4,

    // ==================== Generators/Async ====================
    /// Yield value
    Yield = 0xD0,
    /// Await promise
    Await = 0xD1,
    /// Create async function
    AsyncClosure = 0xD2,
    /// Create generator function
    GeneratorClosure = 0xD3,
    /// Create async generator function
    AsyncGeneratorClosure = 0xD4,

    // ==================== Misc ====================
    /// Move register: dst = src
    Move = 0xE0,
    /// No operation
    Nop = 0xE1,
    /// Debugger statement
    Debugger = 0xE2,
    /// Pop value (discard)
    Pop = 0xE3,
    /// Duplicate top value
    Dup = 0xE4,

    // ==================== Module ====================
    /// Import module
    Import = 0xF0,
    /// Export binding
    Export = 0xF1,
}

impl Opcode {
    /// Convert from raw byte
    pub fn from_byte(byte: u8) -> Option<Self> {
        // Use a match to ensure safety
        match byte {
            0x00 => Some(Self::LoadUndefined),
            0x01 => Some(Self::LoadNull),
            0x02 => Some(Self::LoadTrue),
            0x03 => Some(Self::LoadFalse),
            0x04 => Some(Self::LoadInt8),
            0x05 => Some(Self::LoadInt32),
            0x06 => Some(Self::LoadConst),

            0x10 => Some(Self::GetLocal),
            0x11 => Some(Self::SetLocal),
            0x12 => Some(Self::GetUpvalue),
            0x13 => Some(Self::SetUpvalue),
            0x14 => Some(Self::GetGlobal),
            0x15 => Some(Self::SetGlobal),
            0x16 => Some(Self::LoadThis),
            0x17 => Some(Self::CloseUpvalue),

            0x20 => Some(Self::Add),
            0x21 => Some(Self::Sub),
            0x22 => Some(Self::Mul),
            0x23 => Some(Self::Div),
            0x24 => Some(Self::Mod),
            0x25 => Some(Self::Pow),
            0x26 => Some(Self::Neg),
            0x27 => Some(Self::Inc),
            0x28 => Some(Self::Dec),

            0x30 => Some(Self::BitAnd),
            0x31 => Some(Self::BitOr),
            0x32 => Some(Self::BitXor),
            0x33 => Some(Self::BitNot),
            0x34 => Some(Self::Shl),
            0x35 => Some(Self::Shr),
            0x36 => Some(Self::Ushr),

            0x40 => Some(Self::Eq),
            0x41 => Some(Self::StrictEq),
            0x42 => Some(Self::Ne),
            0x43 => Some(Self::StrictNe),
            0x44 => Some(Self::Lt),
            0x45 => Some(Self::Le),
            0x46 => Some(Self::Gt),
            0x47 => Some(Self::Ge),

            0x50 => Some(Self::Not),

            0x58 => Some(Self::TypeOf),
            0x59 => Some(Self::InstanceOf),
            0x5A => Some(Self::In),
            0x5B => Some(Self::ToNumber),
            0x5C => Some(Self::TypeOfName),
            0x5D => Some(Self::RequireCoercible),

            0x60 => Some(Self::GetProp),
            0x61 => Some(Self::SetProp),
            0x62 => Some(Self::GetPropConst),
            0x63 => Some(Self::SetPropConst),
            0x64 => Some(Self::DeleteProp),
            0x65 => Some(Self::NewObject),
            0x66 => Some(Self::DefineProperty),
            0x67 => Some(Self::DefineGetter),
            0x68 => Some(Self::DefineSetter),

            0x70 => Some(Self::NewArray),
            0x71 => Some(Self::GetElem),
            0x72 => Some(Self::SetElem),
            0x73 => Some(Self::Spread),

            0x80 => Some(Self::Closure),
            0x81 => Some(Self::Call),
            0x82 => Some(Self::CallMethod),
            0x83 => Some(Self::TailCall),
            0x84 => Some(Self::Construct),
            0x85 => Some(Self::Return),
            0x86 => Some(Self::ReturnUndefined),
            0x87 => Some(Self::CallSpread),
            0x88 => Some(Self::ConstructSpread),
            0x89 => Some(Self::CallWithReceiver),
            0x8A => Some(Self::CreateArguments),
            0x8B => Some(Self::CallEval),

            0x90 => Some(Self::Jump),
            0x91 => Some(Self::JumpIfTrue),
            0x92 => Some(Self::JumpIfFalse),
            0x93 => Some(Self::JumpIfNullish),
            0x94 => Some(Self::JumpIfNotNullish),

            0xA0 => Some(Self::TryStart),
            0xA1 => Some(Self::TryEnd),
            0xA2 => Some(Self::Throw),
            0xA3 => Some(Self::Catch),

            0xB0 => Some(Self::GetIterator),
            0xB1 => Some(Self::IteratorNext),
            0xB2 => Some(Self::ForInNext),
            0xB3 => Some(Self::GetAsyncIterator),

            0xC0 => Some(Self::DefineClass),
            0xC1 => Some(Self::GetSuper),
            0xC2 => Some(Self::CallSuper),
            0xC3 => Some(Self::GetSuperProp),
            0xC4 => Some(Self::SetHomeObject),

            0xD0 => Some(Self::Yield),
            0xD1 => Some(Self::Await),
            0xD2 => Some(Self::AsyncClosure),
            0xD3 => Some(Self::GeneratorClosure),
            0xD4 => Some(Self::AsyncGeneratorClosure),

            0xE0 => Some(Self::Move),
            0xE1 => Some(Self::Nop),
            0xE2 => Some(Self::Debugger),
            0xE3 => Some(Self::Pop),
            0xE4 => Some(Self::Dup),

            0xF0 => Some(Self::Import),
            0xF1 => Some(Self::Export),

            _ => None,
        }
    }

    /// Convert to raw byte
    #[inline]
    pub fn to_byte(self) -> u8 {
        self as u8
    }

    /// Get the name of this opcode
    pub const fn name(self) -> &'static str {
        match self {
            // Constants
            Self::LoadUndefined => "LoadUndefined",
            Self::LoadNull => "LoadNull",
            Self::LoadTrue => "LoadTrue",
            Self::LoadFalse => "LoadFalse",
            Self::LoadInt8 => "LoadInt8",
            Self::LoadInt32 => "LoadInt32",
            Self::LoadConst => "LoadConst",
            // Variables
            Self::GetLocal => "GetLocal",
            Self::SetLocal => "SetLocal",
            Self::GetUpvalue => "GetUpvalue",
            Self::SetUpvalue => "SetUpvalue",
            Self::GetGlobal => "GetGlobal",
            Self::SetGlobal => "SetGlobal",
            Self::LoadThis => "LoadThis",
            Self::CloseUpvalue => "CloseUpvalue",
            // Arithmetic
            Self::Add => "Add",
            Self::Sub => "Sub",
            Self::Mul => "Mul",
            Self::Div => "Div",
            Self::Mod => "Mod",
            Self::Pow => "Pow",
            Self::Neg => "Neg",
            Self::Inc => "Inc",
            Self::Dec => "Dec",
            // Bitwise
            Self::BitAnd => "BitAnd",
            Self::BitOr => "BitOr",
            Self::BitXor => "BitXor",
            Self::BitNot => "BitNot",
            Self::Shl => "Shl",
            Self::Shr => "Shr",
            Self::Ushr => "Ushr",
            // Comparison
            Self::Eq => "Eq",
            Self::StrictEq => "StrictEq",
            Self::Ne => "Ne",
            Self::StrictNe => "StrictNe",
            Self::Lt => "Lt",
            Self::Le => "Le",
            Self::Gt => "Gt",
            Self::Ge => "Ge",
            // Logical
            Self::Not => "Not",
            // Type operations
            Self::TypeOf => "TypeOf",
            Self::InstanceOf => "InstanceOf",
            Self::In => "In",
            Self::ToNumber => "ToNumber",
            Self::TypeOfName => "TypeOfName",
            Self::RequireCoercible => "RequireCoercible",
            // Objects
            Self::GetProp => "GetProp",
            Self::SetProp => "SetProp",
            Self::GetPropConst => "GetPropConst",
            Self::SetPropConst => "SetPropConst",
            Self::DeleteProp => "DeleteProp",
            Self::NewObject => "NewObject",
            Self::DefineProperty => "DefineProperty",
            Self::DefineGetter => "DefineGetter",
            Self::DefineSetter => "DefineSetter",
            // Arrays
            Self::NewArray => "NewArray",
            Self::GetElem => "GetElem",
            Self::SetElem => "SetElem",
            Self::Spread => "Spread",
            // Functions
            Self::Closure => "Closure",
            Self::Call => "Call",
            Self::CallMethod => "CallMethod",
            Self::TailCall => "TailCall",
            Self::Construct => "Construct",
            Self::Return => "Return",
            Self::ReturnUndefined => "ReturnUndefined",
            Self::CallSpread => "CallSpread",
            Self::ConstructSpread => "ConstructSpread",
            Self::CallWithReceiver => "CallWithReceiver",
            Self::CreateArguments => "CreateArguments",
            Self::CallEval => "CallEval",
            // Control flow
            Self::Jump => "Jump",
            Self::JumpIfTrue => "JumpIfTrue",
            Self::JumpIfFalse => "JumpIfFalse",
            Self::JumpIfNullish => "JumpIfNullish",
            Self::JumpIfNotNullish => "JumpIfNotNullish",
            // Exception handling
            Self::TryStart => "TryStart",
            Self::TryEnd => "TryEnd",
            Self::Throw => "Throw",
            Self::Catch => "Catch",
            // Iteration
            Self::GetIterator => "GetIterator",
            Self::IteratorNext => "IteratorNext",
            Self::ForInNext => "ForInNext",
            Self::GetAsyncIterator => "GetAsyncIterator",
            // Class
            Self::DefineClass => "DefineClass",
            Self::GetSuper => "GetSuper",
            Self::CallSuper => "CallSuper",
            Self::GetSuperProp => "GetSuperProp",
            Self::SetHomeObject => "SetHomeObject",
            // Generators/Async
            Self::Yield => "Yield",
            Self::Await => "Await",
            Self::AsyncClosure => "AsyncClosure",
            Self::GeneratorClosure => "GeneratorClosure",
            Self::AsyncGeneratorClosure => "AsyncGeneratorClosure",
            // Misc
            Self::Move => "Move",
            Self::Nop => "Nop",
            Self::Debugger => "Debugger",
            Self::Pop => "Pop",
            Self::Dup => "Dup",
            // Module
            Self::Import => "Import",
            Self::Export => "Export",
        }
    }
}

/// A decoded instruction with its operands
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)] // Documentation to be added in future tasks
pub enum Instruction {
    // Constants
    LoadUndefined {
        dst: Register,
    },
    LoadNull {
        dst: Register,
    },
    LoadTrue {
        dst: Register,
    },
    LoadFalse {
        dst: Register,
    },
    LoadInt8 {
        dst: Register,
        value: i8,
    },
    LoadInt32 {
        dst: Register,
        value: i32,
    },
    LoadConst {
        dst: Register,
        idx: ConstantIndex,
    },

    // Variables
    GetLocal {
        dst: Register,
        idx: LocalIndex,
    },
    SetLocal {
        idx: LocalIndex,
        src: Register,
    },
    GetUpvalue {
        dst: Register,
        idx: LocalIndex,
    },
    SetUpvalue {
        idx: LocalIndex,
        src: Register,
    },
    GetGlobal {
        dst: Register,
        name: ConstantIndex,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    SetGlobal {
        name: ConstantIndex,
        src: Register,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    /// Load `this` value into register
    LoadThis {
        dst: Register,
    },
    /// Close upvalue: sync local variable value to its upvalue cell when leaving scope.
    /// This ensures any closures that captured this variable see the final value.
    CloseUpvalue {
        /// Index of the local variable to close
        local_idx: LocalIndex,
    },

    // Arithmetic (generic, with type feedback)
    Add {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    Sub {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    Mul {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    Div {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },

    // Quickened arithmetic (type-specialized, no type checks needed)
    AddI32 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    SubI32 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    MulI32 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    DivI32 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    AddF64 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    SubF64 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    MulF64 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    DivF64 {
        dst: Register,
        lhs: Register,
        rhs: Register,
        feedback_index: u16,
    },
    Mod {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Pow {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Neg {
        dst: Register,
        src: Register,
    },
    Inc {
        dst: Register,
        src: Register,
    },
    Dec {
        dst: Register,
        src: Register,
    },

    // Bitwise
    BitAnd {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    BitOr {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    BitXor {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    BitNot {
        dst: Register,
        src: Register,
    },
    Shl {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Shr {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Ushr {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },

    // Comparison
    Eq {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    StrictEq {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Ne {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    StrictNe {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Lt {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Le {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Gt {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    Ge {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },

    // Logical
    Not {
        dst: Register,
        src: Register,
    },

    // Type operations
    TypeOf {
        dst: Register,
        src: Register,
    },
    TypeOfName {
        dst: Register,
        name: ConstantIndex,
    },
    InstanceOf {
        dst: Register,
        lhs: Register,
        rhs: Register,
        /// Index into the feedback vector for Inline Cache (caches prototype lookup)
        ic_index: u16,
    },
    In {
        dst: Register,
        lhs: Register,
        rhs: Register,
        /// Index into the feedback vector for Inline Cache (caches property existence)
        ic_index: u16,
    },
    /// ToNumber conversion
    ToNumber {
        dst: Register,
        src: Register,
    },
    /// RequireObjectCoercible - throws TypeError if value is null or undefined
    /// Used before destructuring to validate the value can be destructured
    RequireCoercible {
        src: Register,
    },

    // Objects
    GetProp {
        dst: Register,
        obj: Register,
        key: Register,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    SetProp {
        obj: Register,
        key: Register,
        val: Register,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    GetPropConst {
        dst: Register,
        obj: Register,
        name: ConstantIndex,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    SetPropConst {
        obj: Register,
        name: ConstantIndex,
        val: Register,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    DeleteProp {
        dst: Register,
        obj: Register,
        key: Register,
    },
    NewObject {
        dst: Register,
    },
    DefineProperty {
        obj: Register,
        key: Register,
        val: Register,
    },
    /// Define getter on object
    DefineGetter {
        obj: Register,
        key: Register,
        func: Register,
    },
    /// Define setter on object
    DefineSetter {
        obj: Register,
        key: Register,
        func: Register,
    },

    // Arrays
    NewArray {
        dst: Register,
        len: u16,
    },
    GetElem {
        dst: Register,
        arr: Register,
        idx: Register,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    SetElem {
        arr: Register,
        idx: Register,
        val: Register,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    Spread {
        dst: Register,
        src: Register,
    },

    // Functions
    Closure {
        dst: Register,
        func: FunctionIndex,
    },
    Call {
        dst: Register,
        func: Register,
        argc: u8,
    },
    CallMethod {
        dst: Register,
        obj: Register,
        method: ConstantIndex,
        argc: u8,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    /// Create arguments object for current function
    CreateArguments {
        dst: Register,
    },
    /// Call eval: dst = eval(code_reg)
    CallEval {
        dst: Register,
        code: Register,
    },
    CallWithReceiver {
        dst: Register,
        func: Register,
        this: Register,
        argc: u8,
    },
    /// Call method with computed property key: dst = obj[key](...args)
    /// Registers: obj, key, arg1, arg2, ... (contiguous)
    CallMethodComputed {
        dst: Register,
        obj: Register,
        key: Register,
        argc: u8,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },
    TailCall {
        func: Register,
        argc: u8,
    },
    Construct {
        dst: Register,
        func: Register,
        argc: u8,
    },
    Return {
        src: Register,
    },
    ReturnUndefined,
    /// Call function with spread: dst = func(args..., ...spread_arr)
    CallSpread {
        dst: Register,
        func: Register,
        /// Number of regular (non-spread) arguments
        argc: u8,
        /// Register containing the array to spread
        spread: Register,
    },
    /// Construct with spread: dst = new func(args..., ...spread_arr)
    ConstructSpread {
        dst: Register,
        func: Register,
        /// Number of regular (non-spread) arguments
        argc: u8,
        /// Register containing the array to spread
        spread: Register,
    },
    /// Call method with computed key and spread: dst = obj[key](...spread_arr)
    /// The spread array contains all arguments
    CallMethodComputedSpread {
        dst: Register,
        obj: Register,
        key: Register,
        /// Register containing the array of arguments to spread
        spread: Register,
        /// Index into the feedback vector for Inline Cache
        ic_index: u16,
    },

    // Control flow
    Jump {
        offset: JumpOffset,
    },
    JumpIfTrue {
        cond: Register,
        offset: JumpOffset,
    },
    JumpIfFalse {
        cond: Register,
        offset: JumpOffset,
    },
    JumpIfNullish {
        src: Register,
        offset: JumpOffset,
    },
    JumpIfNotNullish {
        src: Register,
        offset: JumpOffset,
    },

    // Exception handling
    TryStart {
        catch_offset: JumpOffset,
    },
    TryEnd,
    Throw {
        src: Register,
    },
    Catch {
        dst: Register,
    },

    // Iteration
    GetIterator {
        dst: Register,
        src: Register,
    },
    GetAsyncIterator {
        dst: Register,
        src: Register,
    },
    IteratorNext {
        dst: Register,
        done: Register,
        iter: Register,
    },
    ForInNext {
        dst: Register,
        obj: Register,
        offset: JumpOffset,
    },

    // Class
    DefineClass {
        dst: Register,
        name: ConstantIndex,
        /// Constructor closure register (already compiled)
        ctor: Register,
        /// Superclass register (None = base class)
        super_class: Option<Register>,
    },
    GetSuper {
        dst: Register,
    },
    CallSuper {
        dst: Register,
        /// Base register where arguments start
        args: Register,
        argc: u8,
    },
    GetSuperProp {
        dst: Register,
        name: ConstantIndex,
    },
    /// Set [[HomeObject]] on a closure: func.home_object = obj
    SetHomeObject {
        func: Register,
        obj: Register,
    },

    // Generators/Async
    Yield {
        dst: Register,
        src: Register,
    },
    Await {
        dst: Register,
        src: Register,
    },
    AsyncClosure {
        dst: Register,
        func: FunctionIndex,
    },
    GeneratorClosure {
        dst: Register,
        func: FunctionIndex,
    },
    AsyncGeneratorClosure {
        dst: Register,
        func: FunctionIndex,
    },

    // Misc
    Move {
        dst: Register,
        src: Register,
    },
    Nop,
    Debugger,
    Pop,
    Dup {
        dst: Register,
        src: Register,
    },

    // Module
    Import {
        dst: Register,
        module: ConstantIndex,
    },
    Export {
        name: ConstantIndex,
        src: Register,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_opcode_roundtrip() {
        let ops = [
            Opcode::LoadUndefined,
            Opcode::Add,
            Opcode::Call,
            Opcode::Jump,
            Opcode::Return,
        ];

        for op in ops {
            let byte = op.to_byte();
            let decoded = Opcode::from_byte(byte);
            assert_eq!(decoded, Some(op));
        }
    }

    #[test]
    fn test_invalid_opcode() {
        assert_eq!(Opcode::from_byte(0xFF), None);
    }

    #[test]
    fn test_opcode_name() {
        assert_eq!(Opcode::Add.name(), "Add");
        assert_eq!(Opcode::LoadUndefined.name(), "LoadUndefined");
        assert_eq!(Opcode::Jump.name(), "Jump");
        assert_eq!(Opcode::Return.name(), "Return");
    }
}
