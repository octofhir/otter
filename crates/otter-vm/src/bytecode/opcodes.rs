//! Opcode enum and static operand shape metadata for bytecode v2.
//!
//! See `docs/bytecode-v2.md` §5 for the full opcode families. Every opcode
//! here has a fixed operand arity and kind list; the *widths* of the
//! operands vary at runtime via the `Wide` / `ExtraWide` prefix bytes.
//!
//! Changes to this file must be mirrored in:
//!
//! - `docs/bytecode-v2.md` §5 tables (source of truth).
//! - `OPCODE_TABLE` below (compile-time shape lookup).
//! - Phase 2's AST lowering (`source_compiler`, TBD).
//! - Phase 3's dispatch (`interpreter/dispatch.rs`, TBD).
//! - Phase 4's JIT codegen (`otter-jit/src/baseline`, TBD).

use super::operand::OperandKind;

/// Every v2 opcode. Discriminants are stable once Phase 1 lands and must
/// only grow upward (new opcodes get fresh values); never reshuffled so
/// that compiled bytecode stays compatible with newer builds of the
/// interpreter.
///
/// Values `0xFE` and `0xFF` are reserved for the `Wide` / `ExtraWide`
/// prefix bytes and are not legal opcode discriminants.
///
/// See `docs/bytecode-v2.md` §5 for the families each opcode belongs to.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Opcode {
    // §5.1 Accumulator load/store
    Ldar = 0x00,
    Star = 0x01,
    Mov = 0x02,
    LdaSmi = 0x03,
    LdaUndefined = 0x04,
    LdaNull = 0x05,
    LdaTrue = 0x06,
    LdaFalse = 0x07,
    LdaTheHole = 0x08,
    LdaNaN = 0x09,
    LdaConstF64 = 0x0A,
    LdaConstStr = 0x0B,
    LdaConstBigInt = 0x0C,
    LdaException = 0x0D,
    LdaNewTarget = 0x0E,
    LdaCurrentClosure = 0x0F,
    LdaThis = 0x10,

    // §5.2 Binary arithmetic (acc = acc OP r)
    Add = 0x11,
    Sub = 0x12,
    Mul = 0x13,
    Div = 0x14,
    Mod = 0x15,
    Exp = 0x16,
    BitwiseAnd = 0x17,
    BitwiseOr = 0x18,
    BitwiseXor = 0x19,
    Shl = 0x1A,
    Shr = 0x1B,
    UShr = 0x1C,

    // §5.2 Smi-immediate arithmetic
    AddSmi = 0x1D,
    SubSmi = 0x1E,
    MulSmi = 0x1F,
    BitwiseOrSmi = 0x20,
    BitwiseAndSmi = 0x21,
    ShlSmi = 0x22,
    ShrSmi = 0x23,

    // §5.2 Unary
    Inc = 0x24,
    Dec = 0x25,
    Negate = 0x26,
    BitwiseNot = 0x27,
    LogicalNot = 0x28,
    TypeOf = 0x29,
    ToBoolean = 0x2A,
    ToNumber = 0x2B,
    ToString = 0x2C,
    ToPropertyKey = 0x2D,

    // §5.3 Comparisons
    TestEqual = 0x2E,
    TestEqualStrict = 0x2F,
    TestLessThan = 0x30,
    TestGreaterThan = 0x31,
    TestLessThanOrEqual = 0x32,
    TestGreaterThanOrEqual = 0x33,
    TestInstanceOf = 0x34,
    TestIn = 0x35,
    TestNull = 0x36,
    TestUndefined = 0x37,
    TestUndetectable = 0x38,
    TestTypeOf = 0x39,
    InPrivate = 0x3A,

    // §5.4 Jumps
    Jump = 0x3B,
    JumpIfTrue = 0x3C,
    JumpIfFalse = 0x3D,
    JumpIfNull = 0x3E,
    JumpIfNotNull = 0x3F,
    JumpIfUndefined = 0x40,
    JumpIfNotUndefined = 0x41,
    JumpIfJSReceiver = 0x42,
    JumpIfToBooleanTrue = 0x43,
    JumpIfToBooleanFalse = 0x44,

    // §5.5 Property access
    LdaNamedProperty = 0x45,
    StaNamedProperty = 0x46,
    LdaKeyedProperty = 0x47,
    StaKeyedProperty = 0x48,
    DelNamedProperty = 0x49,
    DelKeyedProperty = 0x4A,
    LdaGlobal = 0x4B,
    StaGlobal = 0x4C,
    StaGlobalStrict = 0x4D,
    TypeOfGlobal = 0x4E,
    LdaUpvalue = 0x4F,
    StaUpvalue = 0x50,

    // §5.6 Calls
    CallAnyReceiver = 0x51,
    CallProperty = 0x52,
    CallUndefinedReceiver = 0x53,
    CallDirect = 0x54,
    CallSpread = 0x55,
    Construct = 0x56,
    ConstructSpread = 0x57,
    CallEval = 0x58,
    CallSuper = 0x59,
    CallSuperSpread = 0x5A,
    TailCall = 0x5B,

    // §5.7 Control flow
    Return = 0x5C,
    Throw = 0x5D,
    ReThrow = 0x5E,
    Nop = 0x5F,
    Abort = 0x60,

    // §5.8 Generators / async
    Yield = 0x61,
    YieldStar = 0x62,
    SuspendGenerator = 0x63,
    Resume = 0x64,
    Await = 0x65,

    // §5.9 Iteration
    GetIterator = 0x66,
    GetAsyncIterator = 0x67,
    IteratorNext = 0x68,
    IteratorClose = 0x69,
    ForInEnumerate = 0x6A,
    ForInNext = 0x6B,
    SpreadIntoArray = 0x6C,
    ArrayPush = 0x6D,
    CreateEnumerableOwnKeys = 0x6E,
    AssertNotHole = 0x6F,
    AssertConstructor = 0x70,

    // §5.10 Object / array allocation
    CreateObject = 0x71,
    CreateArray = 0x72,
    CreateClosure = 0x73,
    CreateArguments = 0x74,
    CreateRestParameters = 0x75,
    CreateRegExp = 0x76,
    CopyDataProperties = 0x77,
    CopyDataPropertiesExcept = 0x78,
    DefineNamedGetter = 0x79,
    DefineNamedSetter = 0x7A,
    DefineComputedGetter = 0x7B,
    DefineComputedSetter = 0x7C,

    // §5.11 Classes / private / super
    DefineField = 0x7D,
    DefineComputedField = 0x7E,
    RunClassFieldInitializer = 0x7F,
    SetClassFieldInitializer = 0x80,
    AllocClassId = 0x81,
    CopyClassId = 0x82,
    DefinePrivateField = 0x83,
    GetPrivateField = 0x84,
    SetPrivateField = 0x85,
    DefinePrivateMethod = 0x86,
    DefinePrivateGetter = 0x87,
    DefinePrivateSetter = 0x88,
    PushPrivateMethod = 0x89,
    PushPrivateGetter = 0x8A,
    PushPrivateSetter = 0x8B,
    DefineClassMethod = 0x8C,
    DefineClassMethodComputed = 0x8D,
    DefineClassGetter = 0x8E,
    DefineClassSetter = 0x8F,
    DefineClassGetterComputed = 0x90,
    DefineClassSetterComputed = 0x91,
    SetHomeObject = 0x92,
    GetSuperProperty = 0x93,
    GetSuperPropertyComputed = 0x94,
    SetSuperProperty = 0x95,
    SetSuperPropertyComputed = 0x96,
    ThrowConstAssign = 0x97,

    // §5.12 Modules
    DynamicImport = 0x98,
    ImportMeta = 0x99,

    // §15.7.14 Class heritage — wires `Sub.__proto__ = Super` and
    // `Sub.prototype.__proto__ = Super.prototype` per
    // ClassDefinitionEvaluation steps 6-7. Added in M28 so the
    // compiler can set up derived-class prototype chains without
    // depending on the `Object.setPrototypeOf` intrinsic.
    SetClassHeritage = 0x9A,

    // §14.7.5 IteratorStep — mirrors `ForInNext`: drives a single
    // `iterator.next()` step while routing `done` to the
    // accumulator (for `JumpIfTrue`) and the unwrapped `value`
    // to a register operand. Added in M30 to make `for…of`
    // lowerable with just the existing jump opcodes.
    IteratorStep = 0x9B,
    // §14.15 TryStatement / Finally — saves and resumes pending
    // non-throw abrupt completions while `finally` bodies unwind.
    SetPendingReturn = 0x9C,
    SetPendingJump = 0x9D,
    PushPendingFinally = 0x9E,
    ResumeAbrupt = 0x9F,
    // Explicit resource management (`using` / `await using`).
    PushUsingScope = 0xA0,
    AddDisposableResource = 0xA1,
    DisposeUsingScope = 0xA2,
    // 0xFE, 0xFF are reserved for Wide / ExtraWide prefixes. Max legal
    // opcode discriminant is 0xFD.
}

/// Shape metadata for a single opcode: ordered list of operand kinds.
/// Static, compiled into the binary; the width of each operand is decided
/// at decode time by the active prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperandShape {
    operands: &'static [OperandKind],
}

impl OperandShape {
    /// Operands in positional order.
    #[must_use]
    pub const fn operands(&self) -> &'static [OperandKind] {
        self.operands
    }

    /// How many operand slots this instruction reads after the opcode byte.
    #[must_use]
    pub const fn arity(&self) -> usize {
        self.operands.len()
    }

    const fn of(operands: &'static [OperandKind]) -> Self {
        Self { operands }
    }
}

impl Opcode {
    /// Decode a single opcode byte. Returns `None` for prefix bytes
    /// (`0xFE`, `0xFF`) and for any unused discriminant.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        // SAFETY: we fall through the match list and only return Some for
        // values that are actual discriminants of Opcode. The explicit
        // whitelist keeps the code `#![forbid(unsafe_code)]` clean.
        use Opcode::*;
        Some(match byte {
            0x00 => Ldar,
            0x01 => Star,
            0x02 => Mov,
            0x03 => LdaSmi,
            0x04 => LdaUndefined,
            0x05 => LdaNull,
            0x06 => LdaTrue,
            0x07 => LdaFalse,
            0x08 => LdaTheHole,
            0x09 => LdaNaN,
            0x0A => LdaConstF64,
            0x0B => LdaConstStr,
            0x0C => LdaConstBigInt,
            0x0D => LdaException,
            0x0E => LdaNewTarget,
            0x0F => LdaCurrentClosure,
            0x10 => LdaThis,
            0x11 => Add,
            0x12 => Sub,
            0x13 => Mul,
            0x14 => Div,
            0x15 => Mod,
            0x16 => Exp,
            0x17 => BitwiseAnd,
            0x18 => BitwiseOr,
            0x19 => BitwiseXor,
            0x1A => Shl,
            0x1B => Shr,
            0x1C => UShr,
            0x1D => AddSmi,
            0x1E => SubSmi,
            0x1F => MulSmi,
            0x20 => BitwiseOrSmi,
            0x21 => BitwiseAndSmi,
            0x22 => ShlSmi,
            0x23 => ShrSmi,
            0x24 => Inc,
            0x25 => Dec,
            0x26 => Negate,
            0x27 => BitwiseNot,
            0x28 => LogicalNot,
            0x29 => TypeOf,
            0x2A => ToBoolean,
            0x2B => ToNumber,
            0x2C => ToString,
            0x2D => ToPropertyKey,
            0x2E => TestEqual,
            0x2F => TestEqualStrict,
            0x30 => TestLessThan,
            0x31 => TestGreaterThan,
            0x32 => TestLessThanOrEqual,
            0x33 => TestGreaterThanOrEqual,
            0x34 => TestInstanceOf,
            0x35 => TestIn,
            0x36 => TestNull,
            0x37 => TestUndefined,
            0x38 => TestUndetectable,
            0x39 => TestTypeOf,
            0x3A => InPrivate,
            0x3B => Jump,
            0x3C => JumpIfTrue,
            0x3D => JumpIfFalse,
            0x3E => JumpIfNull,
            0x3F => JumpIfNotNull,
            0x40 => JumpIfUndefined,
            0x41 => JumpIfNotUndefined,
            0x42 => JumpIfJSReceiver,
            0x43 => JumpIfToBooleanTrue,
            0x44 => JumpIfToBooleanFalse,
            0x45 => LdaNamedProperty,
            0x46 => StaNamedProperty,
            0x47 => LdaKeyedProperty,
            0x48 => StaKeyedProperty,
            0x49 => DelNamedProperty,
            0x4A => DelKeyedProperty,
            0x4B => LdaGlobal,
            0x4C => StaGlobal,
            0x4D => StaGlobalStrict,
            0x4E => TypeOfGlobal,
            0x4F => LdaUpvalue,
            0x50 => StaUpvalue,
            0x51 => CallAnyReceiver,
            0x52 => CallProperty,
            0x53 => CallUndefinedReceiver,
            0x54 => CallDirect,
            0x55 => CallSpread,
            0x56 => Construct,
            0x57 => ConstructSpread,
            0x58 => CallEval,
            0x59 => CallSuper,
            0x5A => CallSuperSpread,
            0x5B => TailCall,
            0x5C => Return,
            0x5D => Throw,
            0x5E => ReThrow,
            0x5F => Nop,
            0x60 => Abort,
            0x61 => Yield,
            0x62 => YieldStar,
            0x63 => SuspendGenerator,
            0x64 => Resume,
            0x65 => Await,
            0x66 => GetIterator,
            0x67 => GetAsyncIterator,
            0x68 => IteratorNext,
            0x69 => IteratorClose,
            0x6A => ForInEnumerate,
            0x6B => ForInNext,
            0x6C => SpreadIntoArray,
            0x6D => ArrayPush,
            0x6E => CreateEnumerableOwnKeys,
            0x6F => AssertNotHole,
            0x70 => AssertConstructor,
            0x71 => CreateObject,
            0x72 => CreateArray,
            0x73 => CreateClosure,
            0x74 => CreateArguments,
            0x75 => CreateRestParameters,
            0x76 => CreateRegExp,
            0x77 => CopyDataProperties,
            0x78 => CopyDataPropertiesExcept,
            0x79 => DefineNamedGetter,
            0x7A => DefineNamedSetter,
            0x7B => DefineComputedGetter,
            0x7C => DefineComputedSetter,
            0x7D => DefineField,
            0x7E => DefineComputedField,
            0x7F => RunClassFieldInitializer,
            0x80 => SetClassFieldInitializer,
            0x81 => AllocClassId,
            0x82 => CopyClassId,
            0x83 => DefinePrivateField,
            0x84 => GetPrivateField,
            0x85 => SetPrivateField,
            0x86 => DefinePrivateMethod,
            0x87 => DefinePrivateGetter,
            0x88 => DefinePrivateSetter,
            0x89 => PushPrivateMethod,
            0x8A => PushPrivateGetter,
            0x8B => PushPrivateSetter,
            0x8C => DefineClassMethod,
            0x8D => DefineClassMethodComputed,
            0x8E => DefineClassGetter,
            0x8F => DefineClassSetter,
            0x90 => DefineClassGetterComputed,
            0x91 => DefineClassSetterComputed,
            0x92 => SetHomeObject,
            0x93 => GetSuperProperty,
            0x94 => GetSuperPropertyComputed,
            0x95 => SetSuperProperty,
            0x96 => SetSuperPropertyComputed,
            0x97 => ThrowConstAssign,
            0x98 => DynamicImport,
            0x99 => ImportMeta,
            0x9A => SetClassHeritage,
            0x9B => IteratorStep,
            0x9C => SetPendingReturn,
            0x9D => SetPendingJump,
            0x9E => PushPendingFinally,
            0x9F => ResumeAbrupt,
            0xA0 => PushUsingScope,
            0xA1 => AddDisposableResource,
            0xA2 => DisposeUsingScope,
            _ => return None,
        })
    }

    /// Static operand shape for this opcode. Used by the encoder to choose
    /// operand widths and by the decoder to advance the PC cursor.
    #[must_use]
    pub const fn shape(self) -> OperandShape {
        use Opcode::*;
        use OperandKind::*;
        match self {
            // §5.1
            Ldar | Star => OperandShape::of(&[Reg]),
            Mov => OperandShape::of(&[Reg, Reg]),
            LdaSmi => OperandShape::of(&[Imm]),
            LdaUndefined | LdaNull | LdaTrue | LdaFalse | LdaTheHole | LdaNaN | LdaException
            | LdaNewTarget | LdaCurrentClosure | LdaThis => OperandShape::of(&[]),
            LdaConstF64 | LdaConstStr | LdaConstBigInt => OperandShape::of(&[Idx]),

            // §5.2 binary
            Add | Sub | Mul | Div | Mod | Exp | BitwiseAnd | BitwiseOr | BitwiseXor | Shl | Shr
            | UShr => OperandShape::of(&[Reg]),
            // §5.2 Smi variants
            AddSmi | SubSmi | MulSmi | BitwiseOrSmi | BitwiseAndSmi | ShlSmi | ShrSmi => {
                OperandShape::of(&[Imm])
            }
            // §5.2 unary
            Inc | Dec | Negate | BitwiseNot | LogicalNot | TypeOf | ToBoolean | ToNumber
            | ToString | ToPropertyKey => OperandShape::of(&[]),

            // §5.3
            TestEqual
            | TestEqualStrict
            | TestLessThan
            | TestGreaterThan
            | TestLessThanOrEqual
            | TestGreaterThanOrEqual
            | TestInstanceOf
            | TestIn => OperandShape::of(&[Reg]),
            TestNull | TestUndefined | TestUndetectable => OperandShape::of(&[]),
            TestTypeOf => OperandShape::of(&[Imm]),
            InPrivate => OperandShape::of(&[Reg, Idx]),

            // §5.4
            Jump | JumpIfTrue | JumpIfFalse | JumpIfNull | JumpIfNotNull | JumpIfUndefined
            | JumpIfNotUndefined | JumpIfJSReceiver | JumpIfToBooleanTrue
            | JumpIfToBooleanFalse => OperandShape::of(&[JumpOff]),

            // §5.5
            LdaNamedProperty | StaNamedProperty | DelNamedProperty => OperandShape::of(&[Reg, Idx]),
            LdaKeyedProperty | DelKeyedProperty => OperandShape::of(&[Reg]),
            StaKeyedProperty => OperandShape::of(&[Reg, Reg]),
            LdaGlobal | StaGlobal | StaGlobalStrict | TypeOfGlobal | LdaUpvalue | StaUpvalue => {
                OperandShape::of(&[Idx])
            }

            // §5.6
            CallAnyReceiver | CallProperty | CallSpread | CallEval => {
                OperandShape::of(&[Reg, Reg, RegList])
            }
            CallUndefinedReceiver => OperandShape::of(&[Reg, RegList]),
            CallDirect => OperandShape::of(&[Idx, RegList]),
            Construct | ConstructSpread => OperandShape::of(&[Reg, Reg, RegList]),
            CallSuper | CallSuperSpread => OperandShape::of(&[RegList]),
            TailCall => OperandShape::of(&[Reg, Reg, RegList]),

            // §5.7
            Return | Throw | ReThrow | Nop | SetPendingReturn | ResumeAbrupt | PushUsingScope
            | DisposeUsingScope => OperandShape::of(&[]),
            Abort => OperandShape::of(&[Imm]),
            SetPendingJump | PushPendingFinally => OperandShape::of(&[Imm]),
            AddDisposableResource => OperandShape::of(&[Reg, Imm]),

            // §5.8
            Yield | SuspendGenerator | Await => OperandShape::of(&[]),
            YieldStar | Resume => OperandShape::of(&[Reg]),

            // §5.9
            GetIterator
            | GetAsyncIterator
            | IteratorNext
            | IteratorClose
            | ForInEnumerate
            | SpreadIntoArray
            | ArrayPush
            | CreateEnumerableOwnKeys => OperandShape::of(&[Reg]),
            ForInNext => OperandShape::of(&[Reg, Reg]),
            AssertNotHole | AssertConstructor => OperandShape::of(&[]),

            // §5.10
            CreateObject | CreateArray | CreateRestParameters => OperandShape::of(&[]),
            CreateClosure => OperandShape::of(&[Idx, Imm]),
            CreateArguments => OperandShape::of(&[Imm]),
            CreateRegExp => OperandShape::of(&[Idx]),
            CopyDataProperties => OperandShape::of(&[Reg]),
            CopyDataPropertiesExcept => OperandShape::of(&[Reg, RegList]),
            DefineNamedGetter | DefineNamedSetter => OperandShape::of(&[Reg, Idx]),
            DefineComputedGetter | DefineComputedSetter => OperandShape::of(&[Reg, Reg]),

            // §5.11
            DefineField => OperandShape::of(&[Reg, Idx]),
            DefineComputedField => OperandShape::of(&[Reg, Reg]),
            RunClassFieldInitializer | SetClassFieldInitializer => OperandShape::of(&[Reg]),
            AllocClassId => OperandShape::of(&[]),
            CopyClassId => OperandShape::of(&[Reg]),
            DefinePrivateField | GetPrivateField | SetPrivateField | DefinePrivateMethod
            | DefinePrivateGetter | DefinePrivateSetter | PushPrivateMethod | PushPrivateGetter
            | PushPrivateSetter => OperandShape::of(&[Reg, Idx]),
            DefineClassMethod | DefineClassGetter | DefineClassSetter => {
                OperandShape::of(&[Reg, Idx])
            }
            DefineClassMethodComputed | DefineClassGetterComputed | DefineClassSetterComputed => {
                OperandShape::of(&[Reg, Reg])
            }
            SetHomeObject => OperandShape::of(&[Reg, Reg]),
            GetSuperProperty | SetSuperProperty => OperandShape::of(&[Reg, Idx]),
            GetSuperPropertyComputed | SetSuperPropertyComputed => OperandShape::of(&[Reg, Reg]),
            ThrowConstAssign => OperandShape::of(&[]),

            // §5.12
            DynamicImport => OperandShape::of(&[Reg]),
            ImportMeta => OperandShape::of(&[]),

            // §15.7.14 ClassDefinitionEvaluation — heritage wiring.
            SetClassHeritage => OperandShape::of(&[Reg, Reg]),

            // §14.7.5 IteratorStep — (value_reg, iter_reg).
            IteratorStep => OperandShape::of(&[Reg, Reg]),
        }
    }

    /// Raw byte encoding of this opcode.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    /// Human-readable mnemonic (used for disassembly / diagnostics).
    #[must_use]
    pub const fn name(self) -> &'static str {
        use Opcode::*;
        match self {
            Ldar => "Ldar",
            Star => "Star",
            Mov => "Mov",
            LdaSmi => "LdaSmi",
            LdaUndefined => "LdaUndefined",
            LdaNull => "LdaNull",
            LdaTrue => "LdaTrue",
            LdaFalse => "LdaFalse",
            LdaTheHole => "LdaTheHole",
            LdaNaN => "LdaNaN",
            LdaConstF64 => "LdaConstF64",
            LdaConstStr => "LdaConstStr",
            LdaConstBigInt => "LdaConstBigInt",
            LdaException => "LdaException",
            LdaNewTarget => "LdaNewTarget",
            LdaCurrentClosure => "LdaCurrentClosure",
            LdaThis => "LdaThis",
            Add => "Add",
            Sub => "Sub",
            Mul => "Mul",
            Div => "Div",
            Mod => "Mod",
            Exp => "Exp",
            BitwiseAnd => "BitwiseAnd",
            BitwiseOr => "BitwiseOr",
            BitwiseXor => "BitwiseXor",
            Shl => "Shl",
            Shr => "Shr",
            UShr => "UShr",
            AddSmi => "AddSmi",
            SubSmi => "SubSmi",
            MulSmi => "MulSmi",
            BitwiseOrSmi => "BitwiseOrSmi",
            BitwiseAndSmi => "BitwiseAndSmi",
            ShlSmi => "ShlSmi",
            ShrSmi => "ShrSmi",
            Inc => "Inc",
            Dec => "Dec",
            Negate => "Negate",
            BitwiseNot => "BitwiseNot",
            LogicalNot => "LogicalNot",
            TypeOf => "TypeOf",
            ToBoolean => "ToBoolean",
            ToNumber => "ToNumber",
            ToString => "ToString",
            ToPropertyKey => "ToPropertyKey",
            TestEqual => "TestEqual",
            TestEqualStrict => "TestEqualStrict",
            TestLessThan => "TestLessThan",
            TestGreaterThan => "TestGreaterThan",
            TestLessThanOrEqual => "TestLessThanOrEqual",
            TestGreaterThanOrEqual => "TestGreaterThanOrEqual",
            TestInstanceOf => "TestInstanceOf",
            TestIn => "TestIn",
            TestNull => "TestNull",
            TestUndefined => "TestUndefined",
            TestUndetectable => "TestUndetectable",
            TestTypeOf => "TestTypeOf",
            InPrivate => "InPrivate",
            Jump => "Jump",
            JumpIfTrue => "JumpIfTrue",
            JumpIfFalse => "JumpIfFalse",
            JumpIfNull => "JumpIfNull",
            JumpIfNotNull => "JumpIfNotNull",
            JumpIfUndefined => "JumpIfUndefined",
            JumpIfNotUndefined => "JumpIfNotUndefined",
            JumpIfJSReceiver => "JumpIfJSReceiver",
            JumpIfToBooleanTrue => "JumpIfToBooleanTrue",
            JumpIfToBooleanFalse => "JumpIfToBooleanFalse",
            LdaNamedProperty => "LdaNamedProperty",
            StaNamedProperty => "StaNamedProperty",
            LdaKeyedProperty => "LdaKeyedProperty",
            StaKeyedProperty => "StaKeyedProperty",
            DelNamedProperty => "DelNamedProperty",
            DelKeyedProperty => "DelKeyedProperty",
            LdaGlobal => "LdaGlobal",
            StaGlobal => "StaGlobal",
            StaGlobalStrict => "StaGlobalStrict",
            TypeOfGlobal => "TypeOfGlobal",
            LdaUpvalue => "LdaUpvalue",
            StaUpvalue => "StaUpvalue",
            CallAnyReceiver => "CallAnyReceiver",
            CallProperty => "CallProperty",
            CallUndefinedReceiver => "CallUndefinedReceiver",
            CallDirect => "CallDirect",
            CallSpread => "CallSpread",
            Construct => "Construct",
            ConstructSpread => "ConstructSpread",
            CallEval => "CallEval",
            CallSuper => "CallSuper",
            CallSuperSpread => "CallSuperSpread",
            TailCall => "TailCall",
            Return => "Return",
            Throw => "Throw",
            ReThrow => "ReThrow",
            Nop => "Nop",
            Abort => "Abort",
            Yield => "Yield",
            YieldStar => "YieldStar",
            SuspendGenerator => "SuspendGenerator",
            Resume => "Resume",
            Await => "Await",
            GetIterator => "GetIterator",
            GetAsyncIterator => "GetAsyncIterator",
            IteratorNext => "IteratorNext",
            IteratorClose => "IteratorClose",
            ForInEnumerate => "ForInEnumerate",
            ForInNext => "ForInNext",
            SpreadIntoArray => "SpreadIntoArray",
            ArrayPush => "ArrayPush",
            CreateEnumerableOwnKeys => "CreateEnumerableOwnKeys",
            AssertNotHole => "AssertNotHole",
            AssertConstructor => "AssertConstructor",
            CreateObject => "CreateObject",
            CreateArray => "CreateArray",
            CreateClosure => "CreateClosure",
            CreateArguments => "CreateArguments",
            CreateRestParameters => "CreateRestParameters",
            CreateRegExp => "CreateRegExp",
            CopyDataProperties => "CopyDataProperties",
            CopyDataPropertiesExcept => "CopyDataPropertiesExcept",
            DefineNamedGetter => "DefineNamedGetter",
            DefineNamedSetter => "DefineNamedSetter",
            DefineComputedGetter => "DefineComputedGetter",
            DefineComputedSetter => "DefineComputedSetter",
            DefineField => "DefineField",
            DefineComputedField => "DefineComputedField",
            RunClassFieldInitializer => "RunClassFieldInitializer",
            SetClassFieldInitializer => "SetClassFieldInitializer",
            AllocClassId => "AllocClassId",
            CopyClassId => "CopyClassId",
            DefinePrivateField => "DefinePrivateField",
            GetPrivateField => "GetPrivateField",
            SetPrivateField => "SetPrivateField",
            DefinePrivateMethod => "DefinePrivateMethod",
            DefinePrivateGetter => "DefinePrivateGetter",
            DefinePrivateSetter => "DefinePrivateSetter",
            PushPrivateMethod => "PushPrivateMethod",
            PushPrivateGetter => "PushPrivateGetter",
            PushPrivateSetter => "PushPrivateSetter",
            DefineClassMethod => "DefineClassMethod",
            DefineClassMethodComputed => "DefineClassMethodComputed",
            DefineClassGetter => "DefineClassGetter",
            DefineClassSetter => "DefineClassSetter",
            DefineClassGetterComputed => "DefineClassGetterComputed",
            DefineClassSetterComputed => "DefineClassSetterComputed",
            SetHomeObject => "SetHomeObject",
            GetSuperProperty => "GetSuperProperty",
            GetSuperPropertyComputed => "GetSuperPropertyComputed",
            SetSuperProperty => "SetSuperProperty",
            SetSuperPropertyComputed => "SetSuperPropertyComputed",
            ThrowConstAssign => "ThrowConstAssign",
            DynamicImport => "DynamicImport",
            ImportMeta => "ImportMeta",
            SetClassHeritage => "SetClassHeritage",
            IteratorStep => "IteratorStep",
            SetPendingReturn => "SetPendingReturn",
            SetPendingJump => "SetPendingJump",
            PushPendingFinally => "PushPendingFinally",
            ResumeAbrupt => "ResumeAbrupt",
            PushUsingScope => "PushUsingScope",
            AddDisposableResource => "AddDisposableResource",
            DisposeUsingScope => "DisposeUsingScope",
        }
    }

    /// Whether this opcode is a conditional or unconditional branch.
    /// Useful for CFG construction.
    #[must_use]
    pub const fn is_jump(self) -> bool {
        matches!(
            self,
            Opcode::Jump
                | Opcode::JumpIfTrue
                | Opcode::JumpIfFalse
                | Opcode::JumpIfNull
                | Opcode::JumpIfNotNull
                | Opcode::JumpIfUndefined
                | Opcode::JumpIfNotUndefined
                | Opcode::JumpIfJSReceiver
                | Opcode::JumpIfToBooleanTrue
                | Opcode::JumpIfToBooleanFalse
        )
    }

    /// Whether this opcode transfers control without returning (terminator
    /// for a basic block).
    #[must_use]
    pub const fn is_terminator(self) -> bool {
        matches!(
            self,
            Opcode::Return
                | Opcode::Throw
                | Opcode::ReThrow
                | Opcode::Abort
                | Opcode::Jump
                | Opcode::TailCall
        )
    }

    /// Whether dispatch at this PC may suspend the frame (generator /
    /// async boundary). Callers use this to schedule accumulator-save on
    /// suspend paths.
    #[must_use]
    pub const fn is_suspend(self) -> bool {
        matches!(
            self,
            Opcode::Yield | Opcode::YieldStar | Opcode::Await | Opcode::SuspendGenerator
        )
    }
}
