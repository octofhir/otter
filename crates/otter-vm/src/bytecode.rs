//! Compact immutable bytecode for the new VM.

use crate::closure::UpvalueId;
use crate::float::FloatId;
use crate::frame::{FrameLayout, RegisterIndex};
use crate::property::PropertyNameId;
use crate::regexp::RegExpId;
use crate::string::StringId;

/// Program counter type for the new VM bytecode.
pub type ProgramCounter = u32;

/// User-visible register index referenced by bytecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BytecodeRegister(RegisterIndex);

impl BytecodeRegister {
    /// Creates a bytecode-visible register operand.
    #[must_use]
    pub const fn new(index: RegisterIndex) -> Self {
        Self(index)
    }

    /// Returns the register index as encoded in bytecode.
    #[must_use]
    pub const fn index(self) -> RegisterIndex {
        self.0
    }

    /// Resolves the bytecode-visible register into an absolute frame register index.
    #[must_use]
    pub const fn resolve(self, layout: FrameLayout) -> Option<RegisterIndex> {
        layout.resolve_user_visible(self.0)
    }
}

/// Relative bytecode jump offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JumpOffset(i32);

impl JumpOffset {
    /// Creates a relative jump offset.
    #[must_use]
    pub const fn new(offset: i32) -> Self {
        Self(offset)
    }

    /// Returns the raw signed offset.
    #[must_use]
    pub const fn value(self) -> i32 {
        self.0
    }
}

/// Initial opcode set for the new VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Opcode {
    /// No operation.
    Nop = 0x00,
    /// Copy a register: `dst = src`.
    Move = 0x01,
    /// Load a 32-bit integer immediate.
    LoadI32 = 0x02,
    /// Load boolean `true`.
    LoadTrue = 0x03,
    /// Load boolean `false`.
    LoadFalse = 0x04,
    /// Load canonical `NaN`.
    LoadNaN = 0x05,
    /// Allocate a plain object.
    NewObject = 0x06,
    /// Load a string literal from the current function side table.
    LoadString = 0x07,
    /// Allocate a dense array.
    NewArray = 0x08,
    /// Allocate a closure from the current function closure side table.
    NewClosure = 0x09,
    /// Load `undefined`.
    LoadUndefined = 0x0A,
    /// Load `null`.
    LoadNull = 0x0B,
    /// Boolean negation.
    Not = 0x0C,
    /// Load the current pending exception into a register.
    LoadException = 0x0D,
    /// Load the current closure object into a register.
    LoadCurrentClosure = 0x0E,
    /// Load the current receiver / `this` value into a register.
    LoadThis = 0x0F,
    /// JavaScript `typeof`.
    TypeOf = 0x10,
    /// Integer-or-number addition.
    Add = 0x11,
    /// Integer-or-number subtraction.
    Sub = 0x12,
    /// Integer-or-number multiplication.
    Mul = 0x13,
    /// Integer-or-number division.
    Div = 0x14,
    /// Greater-than comparison.
    Gt = 0x15,
    /// Greater-than-or-equal comparison.
    Gte = 0x16,
    /// Less-than-or-equal comparison.
    Lte = 0x17,
    /// Integer-or-number remainder.
    Mod = 0x18,
    /// Load a float64 constant from the float table.
    LoadF64 = 0x19,
    /// Bitwise AND.
    BitAnd = 0x1A,
    /// Bitwise OR.
    BitOr = 0x1B,
    /// Bitwise XOR.
    BitXor = 0x1C,
    /// Left shift.
    Shl = 0x1D,
    /// Signed right shift.
    Shr = 0x1E,
    /// Unsigned right shift.
    UShr = 0x1F,
    /// Equality comparison.
    Eq = 0x20,
    /// Abstract equality comparison.
    LooseEq = 0x21,
    /// Less-than comparison.
    Lt = 0x22,
    /// Load a named property from an object.
    GetProperty = 0x23,
    /// Store a named property on an object.
    SetProperty = 0x24,
    /// Delete a named property from an object.
    DeleteProperty = 0x25,
    /// Load an indexed element from an array or string.
    GetIndex = 0x26,
    /// Store an indexed element on an array.
    SetIndex = 0x27,
    /// Load an upvalue from the current closure context.
    GetUpvalue = 0x28,
    /// Store an upvalue on the current closure context.
    SetUpvalue = 0x29,
    /// Create an internal iterator for a supported iterable.
    GetIterator = 0x2A,
    /// Advance an internal iterator, producing `done` and `value`.
    IteratorNext = 0x2B,
    /// Close an internal iterator.
    IteratorClose = 0x2C,
    /// Load a global variable by name (throws if not found).
    GetGlobal = 0x2D,
    /// `instanceof` operator (ES spec OrdinaryHasInstance).
    InstanceOf = 0x2E,
    /// `in` operator — check if property exists on object.
    HasProperty = 0x2F,
    /// Unconditional jump.
    Jump = 0x30,
    /// Jump if the condition is truthy.
    JumpIfTrue = 0x31,
    /// Jump if the condition is falsy.
    JumpIfFalse = 0x32,
    /// Store a value to a global variable by name.
    SetGlobal = 0x33,
    /// typeof on a global — returns "undefined" for unresolvable references.
    TypeOfGlobal = 0x36,
    /// Create a property key iterator (for..in).
    GetPropertyIterator = 0x34,
    /// Advance a property key iterator; dst_done = bool, dst_key = string key.
    PropertyIteratorNext = 0x35,
    /// Return a register value.
    Return = 0x40,
    /// Call a direct callee with an explicit contiguous argument window.
    CallDirect = 0x41,
    /// Call a closure value with an explicit contiguous argument window.
    CallClosure = 0x42,
    /// Throw the value stored in a register.
    Throw = 0x43,
    /// Await a value (async functions). `dst = await src`.
    /// If the awaited promise is pending, the interpreter suspends.
    Await = 0x44,
    /// Delete a computed (dynamic key) property from an object.
    /// `dst = delete obj[key]` — key is coerced to string at runtime.
    DeleteComputed = 0x45,
    /// ES2024 §10.4.4 CreateArguments — create arguments exotic object from
    /// the current activation's actual arguments (formal params + overflow).
    CreateArguments = 0x46,
    /// ES2024 rest parameter binding — create an Array from overflow arguments.
    CreateRestParameters = 0x47,
    /// Collect enumerable own property keys into a dense Array of strings.
    CreateEnumerableOwnKeys = 0x48,
    /// Load the internal hole sentinel.
    LoadHole = 0x49,
    /// Throw ReferenceError if the source register contains the hole sentinel.
    AssertNotHole = 0x4A,
    /// Define a named getter accessor on an object literal.
    DefineNamedGetter = 0x4B,
    /// Define a named setter accessor on an object literal.
    DefineNamedSetter = 0x4C,
    /// Define a computed getter accessor on an object literal.
    DefineComputedGetter = 0x4D,
    /// Define a computed setter accessor on an object literal.
    DefineComputedSetter = 0x4E,
    /// Copy enumerable own properties from source to target using data-property defines.
    CopyDataProperties = 0x4F,
    /// Copy enumerable own properties from source to target excluding listed keys.
    CopyDataPropertiesExcept = 0x50,
    /// Invoke `super(...)` inside a derived constructor with an explicit argument window.
    CallSuper = 0x51,
    /// Invoke the default derived constructor forwarding all original arguments.
    CallSuperForward = 0x52,
    /// Create an async iterator: look up `Symbol.asyncIterator`, call it; if absent,
    /// fall back to `Symbol.iterator` (sync-to-async wrapping).
    /// `GetAsyncIterator dst, src`
    /// Spec: <https://tc39.es/ecma262/#sec-getiterator> (kind = async)
    GetAsyncIterator = 0x53,
    /// Append a single value to an array (push semantics).
    /// `ArrayPush target_array, value`
    /// Used for array literals with spread elements and spread argument building
    /// where the index is not known at compile time.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-arrayaccumulation>
    ArrayPush = 0x54,
    /// Iterate an iterable and append all elements to an existing array.
    /// `SpreadIntoArray target_array, iterable`
    /// Used for `[...iterable]` and for building spread argument lists.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-arrayaccumulation>
    SpreadIntoArray = 0x56,
    /// Call a function with arguments taken from an array register.
    /// `CallSpread dst, callee, args_array`
    /// Used for `fn(...args)`, `new Fn(...args)`, `super(...args)`.
    /// Spec: <https://tc39.es/ecma262/#sec-runtime-semantics-argumentlistevaluation>
    CallSpread = 0x55,
    /// Invoke `super(...)` with arguments from an array register (spread).
    /// `CallSuperSpread dst, args_array`
    /// Spec: <https://tc39.es/ecma262/#sec-super-keyword-runtime-semantics-evaluation>
    CallSuperSpread = 0x57,
    /// ToNumber conversion.
    ToNumber = 0x5D,
    /// ToString conversion.
    ToString = 0x5E,
    /// Generator yield: suspend execution, produce `value` to caller.
    /// `Yield dst, value` — suspends the generator, returns `value` to the
    /// caller's `.next()`. On resume, the sent value is written to `dst`.
    /// Spec: <https://tc39.es/ecma262/#sec-yield>
    Yield = 0x60,
    /// Allocate a RegExp object from the current function regexp-literal side table.
    /// `NewRegExp dst, regexp_id` — creates a new RegExp instance with the given
    /// pattern and flags, and stores the handle in `dst`.
    /// Spec: <https://tc39.es/ecma262/#sec-regexp-regular-expression-literals>
    NewRegExp = 0x61,
    /// Load a BigInt constant from the constant pool.
    /// `LoadBigInt dst, constant_index` — allocates a BigInt heap value from the
    /// constant pool entry at `constant_index` and stores the handle in `dst`.
    /// Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    LoadBigInt = 0x62,
}

impl Opcode {
    /// Decodes an opcode byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(Self::Nop),
            0x01 => Some(Self::Move),
            0x02 => Some(Self::LoadI32),
            0x03 => Some(Self::LoadTrue),
            0x04 => Some(Self::LoadFalse),
            0x05 => Some(Self::LoadNaN),
            0x06 => Some(Self::NewObject),
            0x07 => Some(Self::LoadString),
            0x08 => Some(Self::NewArray),
            0x09 => Some(Self::NewClosure),
            0x0A => Some(Self::LoadUndefined),
            0x0B => Some(Self::LoadNull),
            0x0C => Some(Self::Not),
            0x0D => Some(Self::LoadException),
            0x0E => Some(Self::LoadCurrentClosure),
            0x0F => Some(Self::LoadThis),
            0x10 => Some(Self::TypeOf),
            0x11 => Some(Self::Add),
            0x12 => Some(Self::Sub),
            0x13 => Some(Self::Mul),
            0x14 => Some(Self::Div),
            0x15 => Some(Self::Gt),
            0x16 => Some(Self::Gte),
            0x17 => Some(Self::Lte),
            0x18 => Some(Self::Mod),
            0x19 => Some(Self::LoadF64),
            0x1A => Some(Self::BitAnd),
            0x1B => Some(Self::BitOr),
            0x1C => Some(Self::BitXor),
            0x1D => Some(Self::Shl),
            0x1E => Some(Self::Shr),
            0x1F => Some(Self::UShr),
            0x20 => Some(Self::Eq),
            0x21 => Some(Self::LooseEq),
            0x22 => Some(Self::Lt),
            0x23 => Some(Self::GetProperty),
            0x24 => Some(Self::SetProperty),
            0x25 => Some(Self::DeleteProperty),
            0x26 => Some(Self::GetIndex),
            0x27 => Some(Self::SetIndex),
            0x28 => Some(Self::GetUpvalue),
            0x29 => Some(Self::SetUpvalue),
            0x2A => Some(Self::GetIterator),
            0x2B => Some(Self::IteratorNext),
            0x2C => Some(Self::IteratorClose),
            0x2D => Some(Self::GetGlobal),
            0x2E => Some(Self::InstanceOf),
            0x2F => Some(Self::HasProperty),
            0x30 => Some(Self::Jump),
            0x31 => Some(Self::JumpIfTrue),
            0x32 => Some(Self::JumpIfFalse),
            0x33 => Some(Self::SetGlobal),
            0x34 => Some(Self::GetPropertyIterator),
            0x35 => Some(Self::PropertyIteratorNext),
            0x36 => Some(Self::TypeOfGlobal),
            0x40 => Some(Self::Return),
            0x41 => Some(Self::CallDirect),
            0x42 => Some(Self::CallClosure),
            0x43 => Some(Self::Throw),
            0x44 => Some(Self::Await),
            0x45 => Some(Self::DeleteComputed),
            0x46 => Some(Self::CreateArguments),
            0x47 => Some(Self::CreateRestParameters),
            0x48 => Some(Self::CreateEnumerableOwnKeys),
            0x49 => Some(Self::LoadHole),
            0x4A => Some(Self::AssertNotHole),
            0x4B => Some(Self::DefineNamedGetter),
            0x4C => Some(Self::DefineNamedSetter),
            0x4D => Some(Self::DefineComputedGetter),
            0x4E => Some(Self::DefineComputedSetter),
            0x4F => Some(Self::CopyDataProperties),
            0x50 => Some(Self::CopyDataPropertiesExcept),
            0x51 => Some(Self::CallSuper),
            0x52 => Some(Self::CallSuperForward),
            0x53 => Some(Self::GetAsyncIterator),
            0x54 => Some(Self::ArrayPush),
            0x55 => Some(Self::CallSpread),
            0x56 => Some(Self::SpreadIntoArray),
            0x57 => Some(Self::CallSuperSpread),
            0x5D => Some(Self::ToNumber),
            0x5E => Some(Self::ToString),
            0x60 => Some(Self::Yield),
            0x61 => Some(Self::NewRegExp),
            0x62 => Some(Self::LoadBigInt),
            _ => None,
        }
    }
}

/// Compact 64-bit runtime instruction.
///
/// Encodings currently used:
///
/// - register/register/register:
///   `opcode:u8 | a:u16 | b:u16 | c:u16 | reserved:u8`
/// - register/immediate:
///   `opcode:u8 | a:u16 | imm32:u32 | reserved:u8`
/// - jump:
///   `opcode:u8 | imm32:i32 | reserved:24`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Instruction(u64);

impl Instruction {
    const OPCODE_SHIFT: u32 = 0;
    const A_SHIFT: u32 = 8;
    const B_SHIFT: u32 = 24;
    const C_SHIFT: u32 = 40;

    /// Encodes a no-op.
    #[must_use]
    pub const fn nop() -> Self {
        Self::from_opcode(Opcode::Nop)
    }

    /// Encodes a move instruction.
    #[must_use]
    pub const fn move_(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Move, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes a 32-bit integer load.
    #[must_use]
    pub const fn load_i32(dst: BytecodeRegister, value: i32) -> Self {
        Self::encode_ai32(Opcode::LoadI32, dst, value)
    }

    /// Encodes a `true` load.
    #[must_use]
    pub const fn load_true(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadTrue,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a `false` load.
    #[must_use]
    pub const fn load_false(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadFalse,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a `NaN` load.
    #[must_use]
    pub const fn load_nan(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadNaN,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an object allocation.
    #[must_use]
    pub const fn new_object(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::NewObject,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a string-literal load.
    #[must_use]
    pub const fn load_string(dst: BytecodeRegister, string: StringId) -> Self {
        Self::encode_abc(
            Opcode::LoadString,
            dst,
            BytecodeRegister::new(string.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a dense-array allocation.
    #[must_use]
    pub const fn new_array(dst: BytecodeRegister, len: u16) -> Self {
        Self::encode_abc(
            Opcode::NewArray,
            dst,
            BytecodeRegister::new(len as RegisterIndex),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a closure allocation using a contiguous capture window.
    #[must_use]
    pub const fn new_closure(dst: BytecodeRegister, capture_start: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::NewClosure,
            dst,
            capture_start,
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an `undefined` load.
    #[must_use]
    pub const fn load_undefined(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadUndefined,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a `null` load.
    #[must_use]
    pub const fn load_null(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadNull,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a pending-exception load.
    #[must_use]
    pub const fn load_exception(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadException,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a current-closure load.
    #[must_use]
    pub const fn load_current_closure(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadCurrentClosure,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a current-receiver / `this` load.
    #[must_use]
    pub const fn load_this(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadThis,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes JavaScript `typeof`.
    #[must_use]
    pub const fn type_of(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::TypeOf, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes `typeof` on a global variable (non-throwing for unresolvable references).
    #[must_use]
    pub const fn type_of_global(dst: BytecodeRegister, name: PropertyNameId) -> Self {
        Self::encode_abc(
            Opcode::TypeOfGlobal,
            dst,
            BytecodeRegister::new(name.0 as RegisterIndex),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a boolean negation.
    #[must_use]
    pub const fn not(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Not, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes a ToNumber conversion.
    #[must_use]
    pub const fn to_number(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::ToNumber, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes a ToString conversion.
    #[must_use]
    pub const fn to_string(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::ToString, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes an addition.
    #[must_use]
    pub const fn add(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Add, dst, lhs, rhs)
    }

    /// Encodes a subtraction.
    #[must_use]
    pub const fn sub(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Sub, dst, lhs, rhs)
    }

    /// Encodes a multiplication.
    #[must_use]
    pub const fn mul(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Mul, dst, lhs, rhs)
    }

    /// Encodes a division.
    #[must_use]
    pub const fn div(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Div, dst, lhs, rhs)
    }

    /// Encodes a greater-than comparison.
    #[must_use]
    pub const fn gt(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Gt, dst, lhs, rhs)
    }

    /// Encodes a greater-than-or-equal comparison.
    #[must_use]
    pub const fn gte(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Gte, dst, lhs, rhs)
    }

    /// Encodes a less-than-or-equal comparison.
    #[must_use]
    pub const fn lte(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Lte, dst, lhs, rhs)
    }

    /// Encodes a remainder/modulo operation.
    #[must_use]
    pub const fn mod_(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Mod, dst, lhs, rhs)
    }

    /// Encodes a bitwise AND operation.
    #[must_use]
    pub const fn bit_and(
        dst: BytecodeRegister,
        lhs: BytecodeRegister,
        rhs: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::BitAnd, dst, lhs, rhs)
    }

    /// Encodes a bitwise OR operation.
    #[must_use]
    pub const fn bit_or(
        dst: BytecodeRegister,
        lhs: BytecodeRegister,
        rhs: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::BitOr, dst, lhs, rhs)
    }

    /// Encodes a bitwise XOR operation.
    #[must_use]
    pub const fn bit_xor(
        dst: BytecodeRegister,
        lhs: BytecodeRegister,
        rhs: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::BitXor, dst, lhs, rhs)
    }

    /// Encodes a left shift operation.
    #[must_use]
    pub const fn shl(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Shl, dst, lhs, rhs)
    }

    /// Encodes a signed right shift operation.
    #[must_use]
    pub const fn shr(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Shr, dst, lhs, rhs)
    }

    /// Encodes an unsigned right shift operation.
    #[must_use]
    pub const fn ushr(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::UShr, dst, lhs, rhs)
    }

    /// Encodes a float64 constant load from the float table.
    #[must_use]
    pub const fn load_f64(dst: BytecodeRegister, float_id: FloatId) -> Self {
        Self::encode_abc(
            Opcode::LoadF64,
            dst,
            BytecodeRegister::new(float_id.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a RegExp-literal allocation from the function's regexp side table.
    /// Spec: <https://tc39.es/ecma262/#sec-regexp-regular-expression-literals>
    #[must_use]
    pub const fn new_regexp(dst: BytecodeRegister, regexp_id: RegExpId) -> Self {
        Self::encode_abc(
            Opcode::NewRegExp,
            dst,
            BytecodeRegister::new(regexp_id.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a BigInt-constant load from the BigInt side table.
    /// Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    #[must_use]
    pub const fn load_bigint(dst: BytecodeRegister, bigint_id: crate::bigint::BigIntId) -> Self {
        Self::encode_abc(
            Opcode::LoadBigInt,
            dst,
            BytecodeRegister::new(bigint_id.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an equality comparison.
    #[must_use]
    pub const fn eq(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Eq, dst, lhs, rhs)
    }

    /// Encodes an abstract equality comparison.
    #[must_use]
    pub const fn loose_eq(
        dst: BytecodeRegister,
        lhs: BytecodeRegister,
        rhs: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::LooseEq, dst, lhs, rhs)
    }

    /// Encodes a less-than comparison.
    #[must_use]
    pub const fn lt(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Lt, dst, lhs, rhs)
    }

    /// Encodes a named property load.
    #[must_use]
    pub const fn get_property(
        dst: BytecodeRegister,
        object: BytecodeRegister,
        property: PropertyNameId,
    ) -> Self {
        Self::encode_abc(
            Opcode::GetProperty,
            dst,
            object,
            BytecodeRegister::new(property.0),
        )
    }

    /// Encodes a named property store.
    #[must_use]
    pub const fn set_property(
        object: BytecodeRegister,
        src: BytecodeRegister,
        property: PropertyNameId,
    ) -> Self {
        Self::encode_abc(
            Opcode::SetProperty,
            object,
            src,
            BytecodeRegister::new(property.0),
        )
    }

    /// Encodes a named property delete.
    #[must_use]
    pub const fn delete_property(
        dst: BytecodeRegister,
        object: BytecodeRegister,
        property: PropertyNameId,
    ) -> Self {
        Self::encode_abc(
            Opcode::DeleteProperty,
            dst,
            object,
            BytecodeRegister::new(property.0),
        )
    }

    /// Encodes a computed property delete. `dst = delete obj[key]`.
    #[must_use]
    pub const fn delete_computed(
        dst: BytecodeRegister,
        object: BytecodeRegister,
        key: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::DeleteComputed, dst, object, key)
    }

    /// ES2024 §10.4.4 CreateArguments — `dst = arguments`.
    #[must_use]
    pub const fn create_arguments(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::CreateArguments,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Rest parameter materialization — `dst = [...overflowArgs]`.
    #[must_use]
    pub const fn create_rest_parameters(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::CreateRestParameters,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Enumerable-own-keys materialization — `dst = EnumerableOwnKeys(src)`.
    #[must_use]
    pub const fn create_enumerable_own_keys(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::CreateEnumerableOwnKeys,
            dst,
            src,
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an internal hole load.
    #[must_use]
    pub const fn load_hole(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadHole,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a hole assertion for TDZ-like binding checks.
    #[must_use]
    pub const fn assert_not_hole(src: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::AssertNotHole,
            src,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Defines a named getter accessor on an object.
    #[must_use]
    pub const fn define_named_getter(
        object: BytecodeRegister,
        getter: BytecodeRegister,
        property: PropertyNameId,
    ) -> Self {
        Self::encode_abc(
            Opcode::DefineNamedGetter,
            object,
            getter,
            BytecodeRegister::new(property.0),
        )
    }

    /// Defines a named setter accessor on an object.
    #[must_use]
    pub const fn define_named_setter(
        object: BytecodeRegister,
        setter: BytecodeRegister,
        property: PropertyNameId,
    ) -> Self {
        Self::encode_abc(
            Opcode::DefineNamedSetter,
            object,
            setter,
            BytecodeRegister::new(property.0),
        )
    }

    /// Defines a computed getter accessor on an object.
    #[must_use]
    pub const fn define_computed_getter(
        object: BytecodeRegister,
        key: BytecodeRegister,
        getter: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::DefineComputedGetter, object, key, getter)
    }

    /// Defines a computed setter accessor on an object.
    #[must_use]
    pub const fn define_computed_setter(
        object: BytecodeRegister,
        key: BytecodeRegister,
        setter: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::DefineComputedSetter, object, key, setter)
    }

    /// Copies enumerable own properties from source to target using `[[DefineOwnProperty]]`.
    #[must_use]
    pub const fn copy_data_properties(target: BytecodeRegister, source: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::CopyDataProperties,
            target,
            source,
            BytecodeRegister::new(0),
        )
    }

    /// Copies enumerable own properties from source to target excluding keys from an array-like.
    #[must_use]
    pub const fn copy_data_properties_except(
        target: BytecodeRegister,
        source: BytecodeRegister,
        excluded_keys: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(
            Opcode::CopyDataPropertiesExcept,
            target,
            source,
            excluded_keys,
        )
    }

    /// Calls `super(...)` with an explicit contiguous argument window.
    #[must_use]
    pub const fn call_super(
        dst: BytecodeRegister,
        args_base: BytecodeRegister,
        argc: RegisterIndex,
    ) -> Self {
        Self::encode_abc(
            Opcode::CallSuper,
            dst,
            args_base,
            BytecodeRegister::new(argc),
        )
    }

    /// Calls the default derived constructor forwarding the original arguments.
    #[must_use]
    pub const fn call_super_forward(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::CallSuperForward,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an indexed load.
    #[must_use]
    pub const fn get_index(
        dst: BytecodeRegister,
        target: BytecodeRegister,
        index: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::GetIndex, dst, target, index)
    }

    /// Encodes an indexed store.
    #[must_use]
    pub const fn set_index(
        target: BytecodeRegister,
        index: BytecodeRegister,
        src: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::SetIndex, target, index, src)
    }

    /// Encodes an iterator allocation.
    #[must_use]
    pub const fn get_iterator(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::GetIterator, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes an async iterator allocation.
    /// Spec: <https://tc39.es/ecma262/#sec-getiterator> (kind = async)
    #[must_use]
    pub const fn get_async_iterator(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::GetAsyncIterator, dst, src, BytecodeRegister::new(0))
    }

    /// Append `value` to `target_array`.
    /// `ArrayPush target_array, value`
    #[must_use]
    pub const fn array_push(
        target_array: BytecodeRegister,
        value: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::ArrayPush, target_array, value, BytecodeRegister::new(0))
    }

    /// Iterate `src` and append all elements to `target_array`.
    /// `SpreadIntoArray target_array, src`
    #[must_use]
    pub const fn spread_into_array(
        target_array: BytecodeRegister,
        src: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::SpreadIntoArray, target_array, src, BytecodeRegister::new(0))
    }

    /// Call `callee` with arguments from `args_array`.
    /// `CallSpread dst, callee, args_array`
    #[must_use]
    pub const fn call_spread(
        dst: BytecodeRegister,
        callee: BytecodeRegister,
        args_array: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::CallSpread, dst, callee, args_array)
    }

    /// Invoke `super(...)` with arguments from an array register.
    /// `CallSuperSpread dst, args_array`
    #[must_use]
    pub const fn call_super_spread(
        dst: BytecodeRegister,
        args_array: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::CallSuperSpread, dst, args_array, BytecodeRegister::new(0))
    }

    /// Encodes one iterator step.
    #[must_use]
    pub const fn iterator_next(
        done_dst: BytecodeRegister,
        value_dst: BytecodeRegister,
        iter: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::IteratorNext, done_dst, value_dst, iter)
    }

    /// Encodes an iterator close.
    #[must_use]
    pub const fn iterator_close(iter: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::IteratorClose,
            iter,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a global variable load (V8's LdaGlobal equivalent).
    #[must_use]
    pub const fn get_global(dst: BytecodeRegister, property: PropertyNameId) -> Self {
        Self::encode_abc(
            Opcode::GetGlobal,
            dst,
            BytecodeRegister::new(property.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a global variable store.
    #[must_use]
    pub const fn set_global(src: BytecodeRegister, property: PropertyNameId) -> Self {
        Self::encode_abc(
            Opcode::SetGlobal,
            src,
            BytecodeRegister::new(property.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a property key iterator creation (for..in).
    #[must_use]
    pub const fn get_property_iterator(dst: BytecodeRegister, object: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::GetPropertyIterator,
            dst,
            object,
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a property iterator advance: dst_done = done?, dst_key = next key.
    #[must_use]
    pub const fn property_iterator_next(
        dst_done: BytecodeRegister,
        dst_key: BytecodeRegister,
        iterator: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::PropertyIteratorNext, dst_done, dst_key, iterator)
    }

    /// Encodes an `instanceof` check.
    #[must_use]
    pub const fn instance_of(
        dst: BytecodeRegister,
        lhs: BytecodeRegister,
        rhs: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::InstanceOf, dst, lhs, rhs)
    }

    /// Encodes an `in` operator check.
    #[must_use]
    pub const fn has_property(
        dst: BytecodeRegister,
        key: BytecodeRegister,
        object: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::HasProperty, dst, key, object)
    }

    /// Encodes an upvalue load.
    #[must_use]
    pub const fn get_upvalue(dst: BytecodeRegister, upvalue: UpvalueId) -> Self {
        Self::encode_abc(
            Opcode::GetUpvalue,
            dst,
            BytecodeRegister::new(upvalue.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an upvalue store.
    #[must_use]
    pub const fn set_upvalue(src: BytecodeRegister, upvalue: UpvalueId) -> Self {
        Self::encode_abc(
            Opcode::SetUpvalue,
            src,
            BytecodeRegister::new(upvalue.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an unconditional jump.
    #[must_use]
    pub const fn jump(offset: JumpOffset) -> Self {
        Self::encode_i32_only(Opcode::Jump, offset.value())
    }

    /// Encodes a conditional jump on truthiness.
    #[must_use]
    pub const fn jump_if_true(cond: BytecodeRegister, offset: JumpOffset) -> Self {
        Self::encode_ai32(Opcode::JumpIfTrue, cond, offset.value())
    }

    /// Encodes a conditional jump on falsiness.
    #[must_use]
    pub const fn jump_if_false(cond: BytecodeRegister, offset: JumpOffset) -> Self {
        Self::encode_ai32(Opcode::JumpIfFalse, cond, offset.value())
    }

    /// Encodes a return instruction.
    #[must_use]
    pub const fn ret(src: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::Return,
            src,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a direct call using the argument window starting at `arg_start`.
    #[must_use]
    pub const fn call_direct(dst: BytecodeRegister, arg_start: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::CallDirect, dst, arg_start, BytecodeRegister::new(0))
    }

    /// Encodes a closure call.
    #[must_use]
    pub const fn call_closure(
        dst: BytecodeRegister,
        callee: BytecodeRegister,
        arg_start: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::CallClosure, dst, callee, arg_start)
    }

    /// Encodes a throw instruction.
    #[must_use]
    pub const fn throw(src: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::Throw,
            src,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an await instruction: `dst = await src`.
    #[must_use]
    pub const fn r#await(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Await, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes a yield instruction: suspend generator, produce `value`.
    /// On resume, the sent value is written to `dst`.
    /// Spec: <https://tc39.es/ecma262/#sec-yield>
    #[must_use]
    pub const fn yield_(dst: BytecodeRegister, value: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Yield, dst, value, BytecodeRegister::new(0))
    }

    /// Returns the decoded opcode.
    #[must_use]
    pub const fn opcode(self) -> Opcode {
        let raw = ((self.0 >> Self::OPCODE_SHIFT) & 0xff) as u8;
        match Opcode::from_byte(raw) {
            Some(opcode) => opcode,
            None => panic!("invalid opcode encoding"),
        }
    }

    /// Returns the first 16-bit operand.
    #[must_use]
    pub const fn a(self) -> u16 {
        ((self.0 >> Self::A_SHIFT) & 0xffff) as u16
    }

    /// Returns the second 16-bit operand.
    #[must_use]
    pub const fn b(self) -> u16 {
        ((self.0 >> Self::B_SHIFT) & 0xffff) as u16
    }

    /// Returns the third 16-bit operand.
    #[must_use]
    pub const fn c(self) -> u16 {
        ((self.0 >> Self::C_SHIFT) & 0xffff) as u16
    }

    /// Returns the 32-bit signed immediate field.
    #[must_use]
    pub const fn immediate_i32(self) -> i32 {
        ((self.0 >> Self::B_SHIFT) & 0xffff_ffff) as u32 as i32
    }

    /// Returns the raw encoded word.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[must_use]
    const fn from_opcode(opcode: Opcode) -> Self {
        Self((opcode as u64) << Self::OPCODE_SHIFT)
    }

    #[must_use]
    const fn encode_abc(
        opcode: Opcode,
        a: BytecodeRegister,
        b: BytecodeRegister,
        c: BytecodeRegister,
    ) -> Self {
        let word = (opcode as u64) << Self::OPCODE_SHIFT
            | (a.index() as u64) << Self::A_SHIFT
            | (b.index() as u64) << Self::B_SHIFT
            | (c.index() as u64) << Self::C_SHIFT;
        Self(word)
    }

    #[must_use]
    const fn encode_ai32(opcode: Opcode, a: BytecodeRegister, immediate: i32) -> Self {
        let word = (opcode as u64) << Self::OPCODE_SHIFT
            | (a.index() as u64) << Self::A_SHIFT
            | ((immediate as u32) as u64) << Self::B_SHIFT;
        Self(word)
    }

    #[must_use]
    const fn encode_i32_only(opcode: Opcode, immediate: i32) -> Self {
        let word =
            (opcode as u64) << Self::OPCODE_SHIFT | ((immediate as u32) as u64) << Self::B_SHIFT;
        Self(word)
    }
}

/// Immutable instruction stream for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bytecode {
    instructions: Box<[Instruction]>,
}

impl Bytecode {
    /// Creates an immutable instruction stream from owned instructions.
    #[must_use]
    pub fn new(instructions: Vec<Instruction>) -> Self {
        Self {
            instructions: instructions.into_boxed_slice(),
        }
    }

    /// Creates an empty immutable instruction stream.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of instructions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Returns `true` when there are no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// Returns the instruction at the given program counter.
    #[must_use]
    pub fn get(&self, pc: ProgramCounter) -> Option<Instruction> {
        self.instructions.get(pc as usize).copied()
    }

    /// Returns the immutable instruction slice.
    #[must_use]
    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }
}

impl Default for Bytecode {
    fn default() -> Self {
        Self::empty()
    }
}

impl From<Vec<Instruction>> for Bytecode {
    fn from(instructions: Vec<Instruction>) -> Self {
        Self::new(instructions)
    }
}

#[cfg(test)]
mod tests {
    use crate::closure::UpvalueId;
    use crate::float::FloatId;
    use crate::frame::FrameLayout;
    use crate::property::PropertyNameId;
    use crate::string::StringId;

    use super::{Bytecode, BytecodeRegister, Instruction, JumpOffset, Opcode};

    #[test]
    fn abc_instruction_round_trips() {
        let instruction = Instruction::add(
            BytecodeRegister::new(1),
            BytecodeRegister::new(2),
            BytecodeRegister::new(3),
        );

        assert_eq!(instruction.opcode(), Opcode::Add);
        assert_eq!(instruction.a(), 1);
        assert_eq!(instruction.b(), 2);
        assert_eq!(instruction.c(), 3);
    }

    #[test]
    fn immediate_instruction_round_trips() {
        let load = Instruction::load_i32(BytecodeRegister::new(4), -17);
        let jump = Instruction::jump(JumpOffset::new(-9));
        let object = Instruction::new_object(BytecodeRegister::new(6));
        let string = Instruction::load_string(BytecodeRegister::new(7), StringId(11));
        let array = Instruction::new_array(BytecodeRegister::new(8), 0);
        let closure = Instruction::new_closure(BytecodeRegister::new(9), BytecodeRegister::new(10));
        let call = Instruction::call_direct(BytecodeRegister::new(9), BytecodeRegister::new(10));
        let exception = Instruction::load_exception(BytecodeRegister::new(12));
        let current_closure = Instruction::load_current_closure(BytecodeRegister::new(13));
        let current_this = Instruction::load_this(BytecodeRegister::new(14));
        let type_of = Instruction::type_of(BytecodeRegister::new(15), BytecodeRegister::new(14));
        let throw = Instruction::throw(BytecodeRegister::new(16));
        let iterator =
            Instruction::get_iterator(BytecodeRegister::new(17), BytecodeRegister::new(18));
        let iterator_next = Instruction::iterator_next(
            BytecodeRegister::new(19),
            BytecodeRegister::new(20),
            BytecodeRegister::new(17),
        );
        let iterator_close = Instruction::iterator_close(BytecodeRegister::new(17));

        assert_eq!(load.opcode(), Opcode::LoadI32);
        assert_eq!(load.a(), 4);
        assert_eq!(load.immediate_i32(), -17);
        assert_eq!(object.opcode(), Opcode::NewObject);
        assert_eq!(object.a(), 6);
        assert_eq!(string.opcode(), Opcode::LoadString);
        assert_eq!(string.a(), 7);
        assert_eq!(string.b(), 11);
        assert_eq!(array.opcode(), Opcode::NewArray);
        assert_eq!(array.a(), 8);
        assert_eq!(closure.opcode(), Opcode::NewClosure);
        assert_eq!(closure.a(), 9);
        assert_eq!(closure.b(), 10);
        assert_eq!(call.opcode(), Opcode::CallDirect);
        assert_eq!(call.a(), 9);
        assert_eq!(call.b(), 10);
        assert_eq!(exception.opcode(), Opcode::LoadException);
        assert_eq!(exception.a(), 12);
        assert_eq!(current_closure.opcode(), Opcode::LoadCurrentClosure);
        assert_eq!(current_closure.a(), 13);
        assert_eq!(current_this.opcode(), Opcode::LoadThis);
        assert_eq!(current_this.a(), 14);
        assert_eq!(type_of.opcode(), Opcode::TypeOf);
        assert_eq!(type_of.a(), 15);
        assert_eq!(type_of.b(), 14);
        assert_eq!(throw.opcode(), Opcode::Throw);
        assert_eq!(throw.a(), 16);
        assert_eq!(iterator.opcode(), Opcode::GetIterator);
        assert_eq!(iterator.a(), 17);
        assert_eq!(iterator.b(), 18);
        assert_eq!(iterator_next.opcode(), Opcode::IteratorNext);
        assert_eq!(iterator_next.a(), 19);
        assert_eq!(iterator_next.b(), 20);
        assert_eq!(iterator_next.c(), 17);
        assert_eq!(iterator_close.opcode(), Opcode::IteratorClose);
        assert_eq!(iterator_close.a(), 17);

        assert_eq!(jump.opcode(), Opcode::Jump);
        assert_eq!(jump.immediate_i32(), -9);
    }

    #[test]
    fn property_instructions_round_trip() {
        let get = Instruction::get_property(
            BytecodeRegister::new(2),
            BytecodeRegister::new(0),
            PropertyNameId(7),
        );
        let delete = Instruction::delete_property(
            BytecodeRegister::new(3),
            BytecodeRegister::new(0),
            PropertyNameId(7),
        );
        let set = Instruction::set_property(
            BytecodeRegister::new(0),
            BytecodeRegister::new(1),
            PropertyNameId(7),
        );
        let get_index = Instruction::get_index(
            BytecodeRegister::new(3),
            BytecodeRegister::new(4),
            BytecodeRegister::new(5),
        );
        let set_index = Instruction::set_index(
            BytecodeRegister::new(4),
            BytecodeRegister::new(5),
            BytecodeRegister::new(6),
        );
        let get_upvalue = Instruction::get_upvalue(BytecodeRegister::new(7), UpvalueId(8));
        let set_upvalue = Instruction::set_upvalue(BytecodeRegister::new(9), UpvalueId(10));
        let define_getter = Instruction::define_named_getter(
            BytecodeRegister::new(10),
            BytecodeRegister::new(11),
            PropertyNameId(12),
        );
        let define_setter = Instruction::define_named_setter(
            BytecodeRegister::new(13),
            BytecodeRegister::new(14),
            PropertyNameId(15),
        );
        let define_computed_getter = Instruction::define_computed_getter(
            BytecodeRegister::new(16),
            BytecodeRegister::new(17),
            BytecodeRegister::new(18),
        );
        let define_computed_setter = Instruction::define_computed_setter(
            BytecodeRegister::new(19),
            BytecodeRegister::new(20),
            BytecodeRegister::new(21),
        );
        let copy_data_properties =
            Instruction::copy_data_properties(BytecodeRegister::new(22), BytecodeRegister::new(23));
        let copy_data_properties_except = Instruction::copy_data_properties_except(
            BytecodeRegister::new(24),
            BytecodeRegister::new(25),
            BytecodeRegister::new(26),
        );
        let call_closure = Instruction::call_closure(
            BytecodeRegister::new(11),
            BytecodeRegister::new(12),
            BytecodeRegister::new(13),
        );

        assert_eq!(get.opcode(), Opcode::GetProperty);
        assert_eq!(get.a(), 2);
        assert_eq!(get.b(), 0);
        assert_eq!(get.c(), 7);

        assert_eq!(delete.opcode(), Opcode::DeleteProperty);
        assert_eq!(delete.a(), 3);
        assert_eq!(delete.b(), 0);
        assert_eq!(delete.c(), 7);

        assert_eq!(set.opcode(), Opcode::SetProperty);
        assert_eq!(set.a(), 0);
        assert_eq!(set.b(), 1);
        assert_eq!(set.c(), 7);

        assert_eq!(get_index.opcode(), Opcode::GetIndex);
        assert_eq!(get_index.a(), 3);
        assert_eq!(get_index.b(), 4);
        assert_eq!(get_index.c(), 5);

        assert_eq!(set_index.opcode(), Opcode::SetIndex);
        assert_eq!(set_index.a(), 4);
        assert_eq!(set_index.b(), 5);
        assert_eq!(set_index.c(), 6);

        assert_eq!(get_upvalue.opcode(), Opcode::GetUpvalue);
        assert_eq!(get_upvalue.a(), 7);
        assert_eq!(get_upvalue.b(), 8);

        assert_eq!(set_upvalue.opcode(), Opcode::SetUpvalue);
        assert_eq!(set_upvalue.a(), 9);
        assert_eq!(set_upvalue.b(), 10);

        assert_eq!(define_getter.opcode(), Opcode::DefineNamedGetter);
        assert_eq!(define_getter.a(), 10);
        assert_eq!(define_getter.b(), 11);
        assert_eq!(define_getter.c(), 12);

        assert_eq!(define_setter.opcode(), Opcode::DefineNamedSetter);
        assert_eq!(define_setter.a(), 13);
        assert_eq!(define_setter.b(), 14);
        assert_eq!(define_setter.c(), 15);

        assert_eq!(
            define_computed_getter.opcode(),
            Opcode::DefineComputedGetter
        );
        assert_eq!(define_computed_getter.a(), 16);
        assert_eq!(define_computed_getter.b(), 17);
        assert_eq!(define_computed_getter.c(), 18);

        assert_eq!(
            define_computed_setter.opcode(),
            Opcode::DefineComputedSetter
        );
        assert_eq!(define_computed_setter.a(), 19);
        assert_eq!(define_computed_setter.b(), 20);
        assert_eq!(define_computed_setter.c(), 21);

        assert_eq!(copy_data_properties.opcode(), Opcode::CopyDataProperties);
        assert_eq!(copy_data_properties.a(), 22);
        assert_eq!(copy_data_properties.b(), 23);
        assert_eq!(copy_data_properties.c(), 0);

        assert_eq!(
            copy_data_properties_except.opcode(),
            Opcode::CopyDataPropertiesExcept
        );
        assert_eq!(copy_data_properties_except.a(), 24);
        assert_eq!(copy_data_properties_except.b(), 25);
        assert_eq!(copy_data_properties_except.c(), 26);

        assert_eq!(call_closure.opcode(), Opcode::CallClosure);
        assert_eq!(call_closure.a(), 11);
        assert_eq!(call_closure.b(), 12);
        assert_eq!(call_closure.c(), 13);
    }

    #[test]
    fn comparison_and_modulo_instructions_round_trip() {
        let gt = Instruction::gt(
            BytecodeRegister::new(5),
            BytecodeRegister::new(3),
            BytecodeRegister::new(4),
        );
        let gte = Instruction::gte(
            BytecodeRegister::new(6),
            BytecodeRegister::new(1),
            BytecodeRegister::new(2),
        );
        let lte = Instruction::lte(
            BytecodeRegister::new(7),
            BytecodeRegister::new(8),
            BytecodeRegister::new(9),
        );
        let mod_ = Instruction::mod_(
            BytecodeRegister::new(10),
            BytecodeRegister::new(11),
            BytecodeRegister::new(12),
        );
        let load_f64 = Instruction::load_f64(BytecodeRegister::new(13), FloatId(42));

        assert_eq!(gt.opcode(), Opcode::Gt);
        assert_eq!(gt.a(), 5);
        assert_eq!(gt.b(), 3);
        assert_eq!(gt.c(), 4);

        assert_eq!(gte.opcode(), Opcode::Gte);
        assert_eq!(gte.a(), 6);
        assert_eq!(gte.b(), 1);
        assert_eq!(gte.c(), 2);

        assert_eq!(lte.opcode(), Opcode::Lte);
        assert_eq!(lte.a(), 7);
        assert_eq!(lte.b(), 8);
        assert_eq!(lte.c(), 9);

        assert_eq!(mod_.opcode(), Opcode::Mod);
        assert_eq!(mod_.a(), 10);
        assert_eq!(mod_.b(), 11);
        assert_eq!(mod_.c(), 12);

        assert_eq!(load_f64.opcode(), Opcode::LoadF64);
        assert_eq!(load_f64.a(), 13);
        assert_eq!(load_f64.b(), 42);
    }

    #[test]
    fn bytecode_register_resolves_against_frame_layout() {
        let layout = FrameLayout::new(2, 3, 4, 5).expect("layout should be valid");
        let register = BytecodeRegister::new(0);
        let last = BytecodeRegister::new(layout.user_visible_count() - 1);

        assert_eq!(register.resolve(layout), Some(layout.user_visible_start()));
        assert_eq!(last.resolve(layout), Some(layout.register_count() - 1));
        assert_eq!(
            BytecodeRegister::new(layout.user_visible_count()).resolve(layout),
            None
        );
    }

    #[test]
    fn bytecode_container_is_immutable_slice() {
        let bytecode = Bytecode::from(vec![
            Instruction::nop(),
            Instruction::ret(BytecodeRegister::new(0)),
        ]);

        assert_eq!(bytecode.len(), 2);
        assert!(!bytecode.is_empty());
        assert_eq!(bytecode.get(0).map(Instruction::opcode), Some(Opcode::Nop));
        assert_eq!(
            bytecode.get(1).map(Instruction::opcode),
            Some(Opcode::Return)
        );
        assert_eq!(bytecode.get(2), None);
    }
}
