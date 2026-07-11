//! Declarative metadata schema for the active bytecode opcode set.
//!
//! # Contents
//! - [`OPCODE_SCHEMA`] is the dense, generated metadata table.
//! - [`OP_BYTE_TABLE`] is the compatibility view consumed by the current wire
//!   encoder and decoder.
//! - [`opcode_schema`] provides an exhaustive `Op` lookup.
//!
//! # Invariants
//! - One macro invocation owns opcode identity and byte assignment; generated
//!   compatibility views cannot drift from it.
//! - The executable format remains self-describing. A first fixed load/local/
//!   arithmetic, property/upvalue/iterator, and representative variadic
//!   families have exact operand/register roles. Branch/return/finally and the
//!   first exception-region successors are exact; remaining rows stay explicit.
//! - Conservative effects never claim a leaf opcode may allocate, throw,
//!   trigger GC, re-enter JavaScript, or require a safepoint.
//!
//! # See also
//! - [`crate::encoding`] for the unchanged executable byte format.
//! - [`crate::opcode_audit`] for the machine-readable schema projection.

use serde::Serialize;

use crate::{NO_HANDLER_OFFSET, Op, Operand};

/// Authority/precision of one schema field family.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataStatus {
    /// The declarative schema is the source of truth.
    SchemaAuthoritative,
    /// The schema owns a deliberately conservative classification.
    SchemaConservative,
}

/// Executable operand encoding format.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperandFormat {
    /// Operand count and kind tags are embedded in each instruction.
    SelfDescribing,
}

/// Wire kind required at one fixed operand position.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperandKind {
    /// A directly encoded register number.
    Register,
    /// An index/count encoded in the unsigned operand form.
    ConstIndex,
    /// A signed immediate value.
    Imm32,
}

impl OperandKind {
    /// Return the wire kind of a decoded operand.
    #[must_use]
    pub const fn of(operand: &Operand) -> Self {
        match operand {
            Operand::Register(_) => Self::Register,
            Operand::ConstIndex(_) => Self::ConstIndex,
            Operand::Imm32(_) => Self::Imm32,
        }
    }
}

/// Register data-flow role carried by an operand.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RegisterAccess {
    /// The operand does not identify a register.
    None,
    /// The instruction reads the identified register.
    Read,
    /// The instruction writes the identified register.
    Write,
}

/// How a register number is represented by an operand.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RegisterSource {
    /// The operand is `Operand::Register`.
    RegisterOperand,
    /// The `Imm32` payload is a register/local index.
    Imm32RegisterIndex,
}

/// One fixed operand position in an authoritative instruction shape.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct OperandSpec {
    /// Required wire kind.
    pub kind: OperandKind,
    /// Register data-flow role.
    pub register_access: RegisterAccess,
    /// Register-number representation when this operand has a data-flow role.
    pub register_source: Option<RegisterSource>,
}

impl OperandSpec {
    const fn value(kind: OperandKind) -> Self {
        Self {
            kind,
            register_access: RegisterAccess::None,
            register_source: None,
        }
    }

    const fn register(access: RegisterAccess) -> Self {
        Self {
            kind: OperandKind::Register,
            register_access: access,
            register_source: Some(RegisterSource::RegisterOperand),
        }
    }

    const fn local_index(access: RegisterAccess) -> Self {
        Self {
            kind: OperandKind::Imm32,
            register_access: access,
            register_source: Some(RegisterSource::Imm32RegisterIndex),
        }
    }
}

/// Precision of an opcode's operand-role declaration.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "status", content = "operands")]
pub enum OperandShape {
    /// Exact fixed operand kinds and register roles.
    Fixed(&'static [OperandSpec]),
    /// Exact fixed prefix followed by a counted homogeneous tail.
    Variadic {
        /// Fixed operands including the count operand.
        prefix: &'static [OperandSpec],
        /// Prefix position containing the unsigned tail count.
        count_operand_index: usize,
        /// Repeated tail operand role.
        tail: OperandSpec,
    },
}

impl OperandShape {
    /// Return exact fixed operands when this row is authoritative.
    #[must_use]
    pub const fn fixed(self) -> Option<&'static [OperandSpec]> {
        match self {
            Self::Fixed(operands) => Some(operands),
            Self::Variadic { .. } => None,
        }
    }

    /// Return the authoritative fixed prefix for fixed or variadic rows.
    #[must_use]
    pub const fn prefix(self) -> Option<&'static [OperandSpec]> {
        match self {
            Self::Fixed(operands)
            | Self::Variadic {
                prefix: operands, ..
            } => Some(operands),
        }
    }

    /// Return counted-tail metadata for an authoritative variadic row.
    #[must_use]
    pub const fn variadic(self) -> Option<(usize, OperandSpec)> {
        match self {
            Self::Variadic {
                count_operand_index,
                tail,
                ..
            } => Some((count_operand_index, tail)),
            Self::Fixed(_) => None,
        }
    }
}

/// Schema validation error for one decoded instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandShapeError {
    /// Fixed operand count differs from the declaration.
    Count {
        /// Declared operand count.
        expected: usize,
        /// Decoded operand count.
        actual: usize,
    },
    /// One decoded operand has the wrong wire kind.
    Kind {
        /// Operand position.
        index: usize,
        /// Declared wire kind.
        expected: OperandKind,
        /// Decoded wire kind.
        actual: OperandKind,
    },
    /// The counted tail cannot be represented in the host index size.
    VariadicCountOverflow {
        /// Decoded unsigned tail count.
        count: u32,
    },
}

impl std::fmt::Display for OperandShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count { expected, actual } => {
                write!(f, "expected {expected} operands, decoded {actual}")
            }
            Self::Kind {
                index,
                expected,
                actual,
            } => write!(
                f,
                "operand {index} expects {expected:?}, decoded {actual:?}"
            ),
            Self::VariadicCountOverflow { count } => {
                write!(
                    f,
                    "variadic operand count {count} overflows instruction size"
                )
            }
        }
    }
}

impl std::error::Error for OperandShapeError {}

/// Current control-flow class. Exact successor PCs remain consumer-decoded.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ControlFlow {
    /// Continue to the next instruction.
    Fallthrough,
    /// Continue only at one encoded target.
    Jump,
    /// Continue at an encoded target or the following instruction.
    Branch,
    /// Invoke a callable and normally resume at the next instruction.
    Call,
    /// Complete the current frame.
    Return,
    /// Unwind with an explicit exception.
    Throw,
    /// Suspend and later resume a frame.
    Suspend,
    /// Mutate or resume structured exception control flow.
    ExceptionRegion,
}

/// Base coordinate used by an encoded relative successor.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RelativeTargetBase {
    /// Byte immediately after the opcode byte (`instruction_pc + 1`).
    AfterOpcode,
}

/// One exact normal control-flow outcome.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SuccessorSpec {
    /// Continue at the next decoded instruction boundary.
    Fallthrough,
    /// Continue at a relative target encoded by an immediate operand.
    RelativeTarget {
        /// Operand position containing the signed byte delta.
        operand_index: usize,
        /// Coordinate from which the byte delta is applied.
        base: RelativeTargetBase,
    },
    /// Complete the current frame without a normal successor PC.
    FrameReturn,
}

/// Precision of an opcode's normal successors.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct SuccessorShape(&'static [SuccessorSpec]);

impl SuccessorShape {
    const fn new(successors: &'static [SuccessorSpec]) -> Self {
        Self(successors)
    }

    /// Return exact normal control-flow outcomes.
    #[must_use]
    pub const fn exact(self) -> &'static [SuccessorSpec] {
        self.0
    }
}

/// One exact exception/unwind control-flow outcome.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum ExceptionSuccessorSpec {
    /// Optional encoded handler target; the sentinel means no edge.
    OptionalRelativeTarget {
        /// Operand position containing the signed byte delta.
        operand_index: usize,
        /// Coordinate from which the byte delta is applied.
        base: RelativeTargetBase,
        /// Immediate value representing an absent handler.
        absent_value: i32,
    },
    /// Unwind to the current frame handler or continue in the caller.
    DynamicFrameHandlerOrCaller,
    /// Resume a parked throw/return/break/continue completion.
    ResumeParkedAbruptCompletion,
    /// Run pending finally handlers down to an encoded handler-stack floor.
    RunFinallyHandlersToFloor {
        /// Operand position containing the non-negative floor.
        floor_operand_index: usize,
    },
}

/// Precision of an opcode's exception/unwind successors.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct ExceptionSuccessorShape(&'static [ExceptionSuccessorSpec]);

impl ExceptionSuccessorShape {
    const fn new(successors: &'static [ExceptionSuccessorSpec]) -> Self {
        Self(successors)
    }

    /// Return exact exception/unwind outcomes.
    #[must_use]
    pub const fn exact(self) -> &'static [ExceptionSuccessorSpec] {
        self.0
    }
}

/// Feedback family currently associated with an opcode.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FeedbackKind {
    /// No feedback cell.
    None,
    /// Arithmetic operand/result feedback.
    Arithmetic,
    /// Named-property feedback.
    Property,
    /// Element or array feedback.
    Element,
    /// Call target/arity feedback.
    Call,
    /// Global or dynamic-environment feedback.
    Global,
}

/// Current machine-code tier coverage policy.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TierSupport {
    /// Native or stub coverage exists for a subset of semantics.
    Partial,
    /// The tier declines compilation or resumes in the interpreter.
    Fallback,
    /// Coverage is available only in the gated experimental tier.
    ExperimentalOnly,
}

/// Conservative execution effects used by tooling and future consumers.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct OpcodeEffects {
    /// The opcode may produce an abrupt exception.
    pub may_throw: bool,
    /// The opcode may allocate managed or external storage.
    pub may_allocate: bool,
    /// The opcode may trigger a moving collection.
    pub may_trigger_gc: bool,
    /// The opcode may invoke user JavaScript through calls or coercions.
    pub may_reenter_javascript: bool,
    /// Compiled slow paths must publish a safepoint.
    pub safepoint_required: bool,
}

/// One generated schema row.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct OpcodeSchema {
    /// Opcode identity.
    pub op: Op,
    /// Stable current wire byte.
    pub byte: u8,
    /// Executable operand encoding format.
    pub operand_format: OperandFormat,
    /// Exact fixed or counted-variadic operand roles.
    pub operand_shape: OperandShape,
    /// Coarse normal control-flow class.
    pub control_flow: ControlFlow,
    /// Exact normal successors.
    pub successor_shape: SuccessorShape,
    /// Exact exception successors.
    pub exception_successor_shape: ExceptionSuccessorShape,
    /// Current feedback-vector family.
    pub feedback: FeedbackKind,
    /// Conservative execution effects.
    pub effects: OpcodeEffects,
    /// Baseline-tier coverage policy.
    pub baseline: TierSupport,
    /// Optimizing-tier coverage policy.
    pub optimizer: TierSupport,
}

impl OpcodeSchema {
    const fn new(op: Op, byte: u8) -> Self {
        Self {
            op,
            byte,
            operand_format: OperandFormat::SelfDescribing,
            operand_shape: operand_shape(op),
            control_flow: control_flow(op),
            successor_shape: successor_shape(op),
            exception_successor_shape: exception_successor_shape(op),
            feedback: feedback(op),
            effects: effects(op),
            baseline: baseline_support(op),
            optimizer: TierSupport::ExperimentalOnly,
        }
    }
}

macro_rules! opcode_schema {
    ($(($op:path, $byte:expr)),+ $(,)?) => {
        /// Dense declarative schema in wire-byte order.
        pub const OPCODE_SCHEMA: &[OpcodeSchema] = &[
            $(OpcodeSchema::new($op, $byte)),+
        ];

        /// Compatibility view for the current encoder/decoder.
        pub const OP_BYTE_TABLE: &[(Op, u8)] = &[
            $(($op, $byte)),+
        ];

        const fn schema_index(op: Op) -> usize {
            match op {
                $($op => $byte as usize),+
            }
        }
    };
}

opcode_schema! {
    (Op::Nop, 0x00),
    (Op::LoadUndefined, 0x01),
    (Op::LoadHole, 0x02),
    (Op::Return, 0x03),
    (Op::LoadString, 0x04),
    (Op::LoadNumber, 0x05),
    (Op::LoadInt32, 0x06),
    (Op::LoadBigInt, 0x07),
    (Op::LoadRegExp, 0x08),
    (Op::QueueMicrotask, 0x09),
    (Op::PromiseNew, 0x0A),
    (Op::PromiseCall, 0x0B),
    (Op::LoadTrue, 0x0C),
    (Op::LoadFalse, 0x0D),
    (Op::LoadLength, 0x0E),
    (Op::GetStringIndex, 0x0F),
    (Op::CallMethodValue, 0x10),
    (Op::Add, 0x11),
    (Op::Sub, 0x12),
    (Op::Mul, 0x13),
    (Op::Div, 0x14),
    (Op::Rem, 0x15),
    (Op::Neg, 0x16),
    (Op::Pow, 0x17),
    (Op::BitwiseAnd, 0x18),
    (Op::BitwiseOr, 0x19),
    (Op::BitwiseXor, 0x1A),
    (Op::BitwiseNot, 0x1B),
    (Op::Shl, 0x1C),
    (Op::Shr, 0x1D),
    (Op::Ushr, 0x1E),
    (Op::ToNumber, 0x1F),
    (Op::Equal, 0x20),
    (Op::NotEqual, 0x21),
    (Op::LessThan, 0x22),
    (Op::LessEq, 0x23),
    (Op::GreaterThan, 0x24),
    (Op::GreaterEq, 0x25),
    (Op::LoadNull, 0x26),
    (Op::LogicalNot, 0x27),
    (Op::ToBoolean, 0x28),
    (Op::Jump, 0x29),
    (Op::JumpIfTrue, 0x2A),
    (Op::JumpIfFalse, 0x2B),
    (Op::JumpIfNullish, 0x2C),
    (Op::LoadLocal, 0x2D),
    (Op::StoreLocal, 0x2E),
    (Op::TdzError, 0x2F),
    (Op::MakeFunction, 0x30),
    (Op::MakeClosure, 0x31),
    (Op::LoadUpvalue, 0x32),
    (Op::StoreUpvalue, 0x33),
    (Op::Call, 0x34),
    (Op::CallWithThis, 0x35),
    (Op::BindFunction, 0x36),
    (Op::LoadThis, 0x37),
    (Op::LoadNewTarget, 0x38),
    (Op::Throw, 0x39),
    (Op::EnterTry, 0x3A),
    (Op::LeaveTry, 0x3B),
    (Op::EndFinally, 0x3C),
    (Op::NewError, 0x3D),
    (Op::GetIterator, 0x3E),
    (Op::IteratorNext, 0x3F),
    (Op::ArrayPush, 0x40),
    (Op::CallSpread, 0x41),
    (Op::New, 0x42),
    (Op::NewSpread, 0x43),
    (Op::SuperConstructSpread, 0x44),
    (Op::MakeClass, 0x45),
    (Op::MathLoad, 0x46),
    (Op::CollectRest, 0x47),
    (Op::ReturnValue, 0x48),
    (Op::ReturnUndefined, 0x49),
    (Op::NewObject, 0x4A),
    (Op::LoadProperty, 0x4B),
    (Op::StoreProperty, 0x4C),
    (Op::DeleteProperty, 0x4D),
    (Op::GetPrototype, 0x4E),
    (Op::SetPrototype, 0x4F),
    (Op::NewArray, 0x50),
    (Op::LoadElement, 0x51),
    (Op::StoreElement, 0x52),
    (Op::ArrayLength, 0x53),
    (Op::HasProperty, 0x54),
    (Op::Instanceof, 0x55),
    (Op::Eval, 0x56),
    (Op::NewFunction, 0x57),
    (Op::LoadGlobalThis, 0x58),
    (Op::LoadGlobalOrThrow, 0x59),
    (Op::CollectArguments, 0x5A),
    (Op::LoadGlobalOrUndefined, 0x5B),
    (Op::DefineGlobalVar, 0x5C),
    (Op::ImportMetaResolve, 0x5D),
    (Op::ImportNamespaceDynamic, 0x5E),
    (Op::ImportNamespace, 0x5F),
    (Op::PromiseFulfilledOf, 0x60),
    (Op::TemporalLoad, 0x61),
    (Op::NewCollection, 0x62),
    (Op::NewWeakRef, 0x63),
    (Op::NewFinalizationRegistry, 0x64),
    (Op::SymbolLoad, 0x65),
    (Op::TypeOf, 0x66),
    (Op::DeleteElement, 0x67),
    (Op::Await, 0x68),
    (Op::SameValue, 0x69),
    (Op::IsArray, 0x6A),
    (Op::LooseEqual, 0x6B),
    (Op::LooseNotEqual, 0x6C),
    (Op::NewBuiltinError, 0x6D),
    (Op::LoadBuiltinError, 0x6E),
    (Op::BigIntCall, 0x6F),
    (Op::ArrayConstruct, 0x70),
    (Op::ArrayFrom, 0x71),
    (Op::ArrayOf, 0x72),
    (Op::ArrayBufferCall, 0x73),
    (Op::DataViewCall, 0x74),
    (Op::Yield, 0x75),
    (Op::SharedArrayBufferCall, 0x76),
    (Op::ToPrimitive, 0x77),
    (Op::ForInKeys, 0x78),
    (Op::CopyDataProperties, 0x79),
    (Op::DefineOwnProperty, 0x7A),
    (Op::IteratorClose, 0x7B),
    (Op::IteratorCloseStart, 0x7C),
    (Op::IteratorCloseEnd, 0x7D),
    (Op::GeneratorStart, 0x7E),
    (Op::GetAsyncIterator, 0x7F),
    (Op::BindThisValue, 0x80),
    (Op::LoadSuperProperty, 0x81),
    (Op::LoadSuperElement, 0x82),
    (Op::SetSuperProperty, 0x83),
    (Op::SetSuperElement, 0x84),
    (Op::JumpViaFinally, 0x85),
    (Op::FreshUpvalue, 0x86),
    (Op::ImportNamespaceDeferred, 0x87),
    (Op::EvaluateModule, 0x88),
    (Op::MarkModuleEvaluated, 0x89),
    (Op::StarReexport, 0x8A),
    (Op::ModuleNamespaceObject, 0x8B),
    (Op::LoadImportBinding, 0x8C),
    (Op::StoreUpvalueChecked, 0x8D),
    (Op::DeclareGlobalVar, 0x8E),
    (Op::LoadDynamic, 0x8F),
    (Op::StoreDynamic, 0x90),
    (Op::TypeofDynamic, 0x91),
    (Op::DefineGlobalFunction, 0x92),
    (Op::DeclareGlobalLex, 0x93),
    (Op::StoreGlobalBinding, 0x94),
    (Op::InitGlobalLex, 0x95),
    (Op::ValidateGlobalDecl, 0x96),
    (Op::ToObject, 0x97),
    (Op::ToNumeric, 0x98),
    (Op::PrivateGet, 0x99),
    (Op::PrivateSet, 0x9A),
    (Op::YieldDelegate, 0x9B),
    (Op::DefineDataProperty, 0x9C),
    (Op::SetFunctionName, 0x9D),
    (Op::ClassCheck, 0x9E),
    (Op::ToPropertyKey, 0x9F),
    (Op::Increment, 0xA0),
    (Op::PrivateBrandCheck, 0xA1),
    (Op::LoadShadowedUpvalue, 0xA2),
    (Op::GetTemplateObject, 0xA3),
    (Op::DeleteDynamic, 0xA4),
    (Op::NewPrivateName, 0xA5),
    (Op::MathCall, 0xA6),
    (Op::TailCall, 0xA7),
    (Op::IsEvalIntrinsic, 0xA8),
    (Op::PopParkedFinally, 0xA9),
    (Op::GlobalBindingExists, 0xAA),
    (Op::StoreGlobalChecked, 0xAB),
}

/// Return the authoritative schema row for `op`.
#[must_use]
pub const fn opcode_schema(op: Op) -> &'static OpcodeSchema {
    &OPCODE_SCHEMA[schema_index(op)]
}

/// Verify exact fixed or counted-tail operand kinds for an authoritative opcode.
/// Transitional rows are accepted without making a precision claim.
///
/// # Errors
/// Returns [`OperandShapeError`] when an authoritative fixed shape does not
/// match the decoded operands.
pub fn verify_operand_shape(op: Op, operands: &[Operand]) -> Result<(), OperandShapeError> {
    let shape = opcode_schema(op).operand_shape;
    let Some(prefix) = shape.prefix() else {
        return Ok(());
    };
    if operands.len() < prefix.len() {
        return Err(OperandShapeError::Count {
            expected: prefix.len(),
            actual: operands.len(),
        });
    }
    verify_operand_specs(&operands[..prefix.len()], prefix, 0)?;
    let Some((count_operand_index, tail)) = shape.variadic() else {
        if operands.len() != prefix.len() {
            return Err(OperandShapeError::Count {
                expected: prefix.len(),
                actual: operands.len(),
            });
        }
        return Ok(());
    };
    let Operand::ConstIndex(count) = operands[count_operand_index] else {
        unreachable!("variadic count kind was checked with its prefix")
    };
    let expected_len = prefix
        .len()
        .checked_add(count as usize)
        .ok_or(OperandShapeError::VariadicCountOverflow { count })?;
    if operands.len() != expected_len {
        return Err(OperandShapeError::Count {
            expected: expected_len,
            actual: operands.len(),
        });
    }
    verify_operand_specs(&operands[prefix.len()..], &[tail], prefix.len())
}

fn verify_operand_specs(
    operands: &[Operand],
    expected: &[OperandSpec],
    index_base: usize,
) -> Result<(), OperandShapeError> {
    for (offset, operand) in operands.iter().enumerate() {
        let spec = expected
            .get(offset % expected.len())
            .expect("operand spec list is non-empty when operands are present");
        let actual = OperandKind::of(operand);
        if actual != spec.kind {
            return Err(OperandShapeError::Kind {
                index: index_base + offset,
                expected: spec.kind,
                actual,
            });
        }
    }
    Ok(())
}

const W: OperandSpec = OperandSpec::register(RegisterAccess::Write);
const R: OperandSpec = OperandSpec::register(RegisterAccess::Read);
const IMM: OperandSpec = OperandSpec::value(OperandKind::Imm32);
const CONST: OperandSpec = OperandSpec::value(OperandKind::ConstIndex);
const LOCAL_R: OperandSpec = OperandSpec::local_index(RegisterAccess::Read);
const LOCAL_W: OperandSpec = OperandSpec::local_index(RegisterAccess::Write);

const EMPTY: &[OperandSpec] = &[];
const WRITE: &[OperandSpec] = &[W];
const WRITE_CONST: &[OperandSpec] = &[W, CONST];
const WRITE_IMM: &[OperandSpec] = &[W, IMM];
const WRITE_READ: &[OperandSpec] = &[W, R];
const WRITE_READ_READ: &[OperandSpec] = &[W, R, R];
const LOAD_LOCAL: &[OperandSpec] = &[W, LOCAL_R];
const STORE_LOCAL: &[OperandSpec] = &[R, LOCAL_W];
const JUMP: &[OperandSpec] = &[IMM];
const BRANCH: &[OperandSpec] = &[IMM, R];
const CALL_PREFIX: &[OperandSpec] = &[W, R, CONST];
const CALL_WITH_THIS_PREFIX: &[OperandSpec] = &[W, R, R, CONST];
const COUNTED_VALUES_PREFIX: &[OperandSpec] = &[W, CONST];
const METHOD_CALL_PREFIX: &[OperandSpec] = &[W, R, CONST, CONST];
const NAMESPACE_CALL_PREFIX: &[OperandSpec] = &[W, CONST, CONST];
const ENTER_TRY: &[OperandSpec] = &[IMM, IMM, W];
const WRITE_READ_CONST: &[OperandSpec] = &[W, R, CONST];
const WRITE_CONST_CONST: &[OperandSpec] = &[W, CONST, CONST];
const READ_CONST_READ_WRITE: &[OperandSpec] = &[R, CONST, R, W];
const READ_READ: &[OperandSpec] = &[R, R];
const READ_READ_READ_WRITE: &[OperandSpec] = &[R, R, R, W];
const WRITE_WRITE_READ: &[OperandSpec] = &[W, W, R];
const WRITE_CONST_IMM: &[OperandSpec] = &[W, CONST, IMM];
const JUMP_VIA_FINALLY: &[OperandSpec] = &[IMM, IMM];
const READ_CONST: &[OperandSpec] = &[R, CONST];
const CONST_READ: &[OperandSpec] = &[CONST, R];
const WRITE_READ_WRITE: &[OperandSpec] = &[W, R, W];
const WRITE_READ_READ_READ: &[OperandSpec] = &[W, R, R, R];
const WRITE_FOUR_READS: &[OperandSpec] = &[W, R, R, R, R];
const WRITE_READ_IMM: &[OperandSpec] = &[W, R, IMM];
const CONST_READ_IMM: &[OperandSpec] = &[CONST, R, IMM];
const CONST_IMM: &[OperandSpec] = &[CONST, IMM];
const READ_CONST_IMM: &[OperandSpec] = &[R, CONST, IMM];

const fn operand_shape(op: Op) -> OperandShape {
    match op {
        Op::Nop => OperandShape::Fixed(EMPTY),
        Op::LoadUndefined
        | Op::LoadHole
        | Op::LoadNull
        | Op::LoadTrue
        | Op::LoadFalse
        | Op::LoadThis
        | Op::LoadNewTarget
        | Op::LoadGlobalThis => OperandShape::Fixed(WRITE),
        Op::LoadString | Op::LoadNumber | Op::LoadBigInt | Op::LoadRegExp => {
            OperandShape::Fixed(WRITE_CONST)
        }
        Op::LoadInt32 => OperandShape::Fixed(WRITE_IMM),
        Op::Jump => OperandShape::Fixed(JUMP),
        Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => OperandShape::Fixed(BRANCH),
        Op::Return | Op::ReturnValue => OperandShape::Fixed(&[R]),
        Op::ReturnUndefined => OperandShape::Fixed(EMPTY),
        Op::Call | Op::TailCall => OperandShape::Variadic {
            prefix: CALL_PREFIX,
            count_operand_index: 2,
            tail: R,
        },
        Op::CallWithThis => OperandShape::Variadic {
            prefix: CALL_WITH_THIS_PREFIX,
            count_operand_index: 3,
            tail: R,
        },
        Op::New => OperandShape::Variadic {
            prefix: CALL_PREFIX,
            count_operand_index: 2,
            tail: R,
        },
        Op::BindFunction => OperandShape::Variadic {
            prefix: CALL_WITH_THIS_PREFIX,
            count_operand_index: 3,
            tail: R,
        },
        Op::NewArray | Op::ArrayConstruct | Op::ArrayFrom | Op::ArrayOf | Op::NewFunction => {
            OperandShape::Variadic {
                prefix: COUNTED_VALUES_PREFIX,
                count_operand_index: 1,
                tail: R,
            }
        }
        Op::CallMethodValue => OperandShape::Variadic {
            prefix: METHOD_CALL_PREFIX,
            count_operand_index: 3,
            tail: R,
        },
        Op::MathCall => OperandShape::Variadic {
            prefix: NAMESPACE_CALL_PREFIX,
            count_operand_index: 2,
            tail: R,
        },
        Op::Throw => OperandShape::Fixed(&[R]),
        Op::EnterTry => OperandShape::Fixed(ENTER_TRY),
        Op::EndFinally => OperandShape::Fixed(EMPTY),
        Op::NewObject => OperandShape::Fixed(WRITE),
        Op::LoadProperty | Op::DeleteProperty => OperandShape::Fixed(WRITE_READ_CONST),
        Op::StoreProperty => OperandShape::Fixed(READ_CONST_READ_WRITE),
        Op::GetPrototype | Op::ArrayLength | Op::GetIterator | Op::GetAsyncIterator => {
            OperandShape::Fixed(WRITE_READ)
        }
        Op::SetPrototype | Op::ArrayPush | Op::CopyDataProperties => OperandShape::Fixed(READ_READ),
        Op::LoadElement | Op::DeleteElement | Op::HasProperty | Op::Instanceof => {
            OperandShape::Fixed(WRITE_READ_READ)
        }
        Op::StoreElement => OperandShape::Fixed(READ_READ_READ_WRITE),
        Op::IteratorNext => OperandShape::Fixed(WRITE_WRITE_READ),
        Op::IteratorClose | Op::IteratorCloseStart | Op::IteratorCloseEnd => {
            OperandShape::Fixed(&[R])
        }
        Op::ForInKeys => OperandShape::Fixed(WRITE_READ),
        Op::LoadUpvalue => OperandShape::Fixed(WRITE_IMM),
        Op::StoreUpvalue | Op::StoreUpvalueChecked => OperandShape::Fixed(&[R, IMM]),
        Op::FreshUpvalue => OperandShape::Fixed(&[IMM]),
        Op::LoadShadowedUpvalue => OperandShape::Fixed(WRITE_CONST_IMM),
        Op::JumpViaFinally => OperandShape::Fixed(JUMP_VIA_FINALLY),
        Op::PopParkedFinally => OperandShape::Fixed(&[IMM]),
        Op::QueueMicrotask => OperandShape::Variadic {
            prefix: &[R, CONST],
            count_operand_index: 1,
            tail: R,
        },
        Op::PromiseNew => OperandShape::Fixed(WRITE_READ_WRITE),
        Op::PromiseCall
        | Op::BigIntCall
        | Op::ArrayBufferCall
        | Op::DataViewCall
        | Op::SharedArrayBufferCall => OperandShape::Variadic {
            prefix: NAMESPACE_CALL_PREFIX,
            count_operand_index: 2,
            tail: R,
        },
        Op::LoadLength | Op::TypeOf | Op::Await | Op::IsArray | Op::IsEvalIntrinsic => {
            OperandShape::Fixed(WRITE_READ)
        }
        Op::GetStringIndex | Op::SameValue | Op::LooseEqual | Op::LooseNotEqual => {
            OperandShape::Fixed(WRITE_READ_READ)
        }
        Op::TdzError => OperandShape::Fixed(&[IMM]),
        Op::MakeFunction
        | Op::MathLoad
        | Op::LoadGlobalOrThrow
        | Op::LoadGlobalOrUndefined
        | Op::ImportNamespace
        | Op::ImportNamespaceDeferred
        | Op::ModuleNamespaceObject
        | Op::TemporalLoad
        | Op::SymbolLoad
        | Op::LoadBuiltinError
        | Op::GetTemplateObject
        | Op::NewPrivateName
        | Op::EvaluateModule => OperandShape::Fixed(WRITE_CONST),
        Op::MakeClosure => OperandShape::Variadic {
            prefix: NAMESPACE_CALL_PREFIX,
            count_operand_index: 2,
            tail: IMM,
        },
        Op::LeaveTry | Op::GeneratorStart => OperandShape::Fixed(EMPTY),
        Op::NewError
        | Op::ImportMetaResolve
        | Op::ImportNamespaceDynamic
        | Op::PromiseFulfilledOf
        | Op::NewWeakRef
        | Op::NewFinalizationRegistry
        | Op::Yield => OperandShape::Fixed(WRITE_READ),
        Op::CallSpread => OperandShape::Fixed(WRITE_READ_READ_READ),
        Op::NewSpread | Op::SuperConstructSpread => OperandShape::Fixed(WRITE_READ_READ),
        Op::MakeClass => OperandShape::Fixed(WRITE_FOUR_READS),
        Op::CollectRest | Op::CollectArguments => OperandShape::Fixed(WRITE),
        Op::Eval | Op::Increment => OperandShape::Fixed(WRITE_READ_IMM),
        Op::DefineGlobalVar => OperandShape::Fixed(CONST_READ),
        Op::NewCollection | Op::NewBuiltinError => OperandShape::Fixed(&[W, CONST, R]),
        Op::ToPrimitive => OperandShape::Fixed(WRITE_READ_CONST),
        Op::DefineOwnProperty | Op::PrivateSet | Op::DefineDataProperty => {
            OperandShape::Fixed(&[R, R, R])
        }
        Op::YieldDelegate => OperandShape::Fixed(WRITE_WRITE_READ),
        Op::SetFunctionName => OperandShape::Fixed(&[R, R, CONST]),
        Op::StoreGlobalChecked => OperandShape::Fixed(&[R, CONST, R]),
        Op::BindThisValue => OperandShape::Fixed(&[R]),
        Op::LoadSuperProperty => OperandShape::Fixed(WRITE_READ_CONST),
        Op::LoadSuperElement => OperandShape::Fixed(WRITE_READ_READ),
        Op::SetSuperProperty => OperandShape::Fixed(&[R, CONST, R]),
        Op::SetSuperElement => OperandShape::Fixed(&[R, R, R]),
        Op::MarkModuleEvaluated => OperandShape::Fixed(&[CONST]),
        Op::DeclareGlobalVar => OperandShape::Fixed(CONST_IMM),
        Op::StarReexport => OperandShape::Fixed(READ_READ),
        Op::LoadImportBinding => OperandShape::Fixed(WRITE_CONST_CONST),
        Op::LoadDynamic | Op::TypeofDynamic | Op::DeleteDynamic => OperandShape::Fixed(WRITE_CONST),
        Op::StoreDynamic | Op::InitGlobalLex => OperandShape::Fixed(READ_CONST),
        Op::DefineGlobalFunction => OperandShape::Fixed(CONST_READ_IMM),
        Op::DeclareGlobalLex | Op::ValidateGlobalDecl => OperandShape::Fixed(CONST_IMM),
        Op::StoreGlobalBinding => OperandShape::Fixed(READ_CONST_IMM),
        Op::ToObject | Op::ToNumeric | Op::ToPropertyKey => OperandShape::Fixed(WRITE_READ),
        Op::ClassCheck => OperandShape::Fixed(&[IMM, R]),
        Op::PrivateGet => OperandShape::Fixed(WRITE_READ_READ),
        Op::PrivateBrandCheck => OperandShape::Fixed(READ_READ),
        Op::GlobalBindingExists => OperandShape::Fixed(WRITE_CONST),
        Op::LoadLocal => OperandShape::Fixed(LOAD_LOCAL),
        Op::StoreLocal => OperandShape::Fixed(STORE_LOCAL),
        Op::Neg | Op::BitwiseNot | Op::ToNumber | Op::LogicalNot | Op::ToBoolean => {
            OperandShape::Fixed(WRITE_READ)
        }
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Pow
        | Op::BitwiseAnd
        | Op::BitwiseOr
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::Ushr
        | Op::Equal
        | Op::NotEqual
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq => OperandShape::Fixed(WRITE_READ_READ),
    }
}

const RELATIVE_TARGET: SuccessorSpec = SuccessorSpec::RelativeTarget {
    operand_index: 0,
    base: RelativeTargetBase::AfterOpcode,
};
const JUMP_SUCCESSORS: &[SuccessorSpec] = &[RELATIVE_TARGET];
const BRANCH_SUCCESSORS: &[SuccessorSpec] = &[RELATIVE_TARGET, SuccessorSpec::Fallthrough];
const RETURN_SUCCESSORS: &[SuccessorSpec] = &[SuccessorSpec::FrameReturn];
const TAIL_CALL_SUCCESSORS: &[SuccessorSpec] =
    &[SuccessorSpec::FrameReturn, SuccessorSpec::Fallthrough];
const FALLTHROUGH_SUCCESSORS: &[SuccessorSpec] = &[SuccessorSpec::Fallthrough];
const NO_NORMAL_SUCCESSORS: &[SuccessorSpec] = &[];

const fn successor_shape(op: Op) -> SuccessorShape {
    match op {
        Op::Jump => SuccessorShape::new(JUMP_SUCCESSORS),
        Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => {
            SuccessorShape::new(BRANCH_SUCCESSORS)
        }
        Op::Return | Op::ReturnValue | Op::ReturnUndefined => {
            SuccessorShape::new(RETURN_SUCCESSORS)
        }
        Op::TailCall => SuccessorShape::new(TAIL_CALL_SUCCESSORS),
        Op::EnterTry | Op::LeaveTry | Op::EndFinally | Op::PopParkedFinally => {
            SuccessorShape::new(FALLTHROUGH_SUCCESSORS)
        }
        Op::Throw => SuccessorShape::new(NO_NORMAL_SUCCESSORS),
        Op::JumpViaFinally => SuccessorShape::new(JUMP_SUCCESSORS),
        Op::Await | Op::Yield | Op::YieldDelegate | Op::GeneratorStart => {
            SuccessorShape::new(FALLTHROUGH_SUCCESSORS)
        }
        _ => match control_flow(op) {
            ControlFlow::Fallthrough | ControlFlow::Call => {
                SuccessorShape::new(FALLTHROUGH_SUCCESSORS)
            }
            ControlFlow::Jump
            | ControlFlow::Branch
            | ControlFlow::Return
            | ControlFlow::Throw
            | ControlFlow::Suspend
            | ControlFlow::ExceptionRegion => unreachable!(),
        },
    }
}

const ENTER_TRY_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] = &[
    ExceptionSuccessorSpec::OptionalRelativeTarget {
        operand_index: 0,
        base: RelativeTargetBase::AfterOpcode,
        absent_value: NO_HANDLER_OFFSET,
    },
    ExceptionSuccessorSpec::OptionalRelativeTarget {
        operand_index: 1,
        base: RelativeTargetBase::AfterOpcode,
        absent_value: NO_HANDLER_OFFSET,
    },
];
const THROW_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] =
    &[ExceptionSuccessorSpec::DynamicFrameHandlerOrCaller];
const END_FINALLY_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] =
    &[ExceptionSuccessorSpec::ResumeParkedAbruptCompletion];
const JUMP_VIA_FINALLY_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] =
    &[ExceptionSuccessorSpec::RunFinallyHandlersToFloor {
        floor_operand_index: 1,
    }];
const NO_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] = &[];

const fn exception_successor_shape(op: Op) -> ExceptionSuccessorShape {
    match op {
        Op::EnterTry => ExceptionSuccessorShape::new(ENTER_TRY_EXCEPTION_SUCCESSORS),
        Op::Throw => ExceptionSuccessorShape::new(THROW_EXCEPTION_SUCCESSORS),
        Op::EndFinally => ExceptionSuccessorShape::new(END_FINALLY_EXCEPTION_SUCCESSORS),
        Op::JumpViaFinally => ExceptionSuccessorShape::new(JUMP_VIA_FINALLY_EXCEPTION_SUCCESSORS),
        Op::PopParkedFinally => ExceptionSuccessorShape::new(NO_EXCEPTION_SUCCESSORS),
        _ if !effects(op).may_throw => ExceptionSuccessorShape::new(NO_EXCEPTION_SUCCESSORS),
        _ => ExceptionSuccessorShape::new(THROW_EXCEPTION_SUCCESSORS),
    }
}

const fn control_flow(op: Op) -> ControlFlow {
    match op {
        Op::Jump | Op::JumpViaFinally => ControlFlow::Jump,
        Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => ControlFlow::Branch,
        Op::Return | Op::ReturnValue | Op::ReturnUndefined | Op::TailCall => ControlFlow::Return,
        Op::Throw => ControlFlow::Throw,
        Op::EnterTry | Op::LeaveTry | Op::EndFinally | Op::PopParkedFinally => {
            ControlFlow::ExceptionRegion
        }
        Op::Await | Op::Yield | Op::YieldDelegate | Op::GeneratorStart => ControlFlow::Suspend,
        Op::Call
        | Op::CallWithThis
        | Op::CallMethodValue
        | Op::CallSpread
        | Op::New
        | Op::NewSpread
        | Op::SuperConstructSpread
        | Op::Eval
        | Op::MathCall
        | Op::PromiseCall => ControlFlow::Call,
        _ => ControlFlow::Fallthrough,
    }
}

const fn feedback(op: Op) -> FeedbackKind {
    match op {
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Pow
        | Op::Increment
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq => FeedbackKind::Arithmetic,
        Op::LoadProperty | Op::StoreProperty | Op::HasProperty | Op::DeleteProperty => {
            FeedbackKind::Property
        }
        Op::LoadElement | Op::StoreElement | Op::DeleteElement | Op::ArrayLength => {
            FeedbackKind::Element
        }
        Op::Call | Op::CallWithThis | Op::CallMethodValue | Op::TailCall => FeedbackKind::Call,
        Op::LoadGlobalOrThrow
        | Op::LoadGlobalOrUndefined
        | Op::StoreGlobalBinding
        | Op::LoadDynamic
        | Op::StoreDynamic => FeedbackKind::Global,
        _ => FeedbackKind::None,
    }
}

const fn effects(op: Op) -> OpcodeEffects {
    let leaf = matches!(
        op,
        Op::Nop
            | Op::LoadUndefined
            | Op::LoadHole
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::LoadNull
            | Op::LoadLocal
            | Op::StoreLocal
            | Op::LoadThis
            | Op::LoadNewTarget
            | Op::Jump
            | Op::JumpIfTrue
            | Op::JumpIfFalse
            | Op::JumpIfNullish
            | Op::LeaveTry
            | Op::Return
            | Op::ReturnValue
            | Op::ReturnUndefined
    );
    OpcodeEffects {
        may_throw: !leaf,
        may_allocate: !leaf,
        may_trigger_gc: !leaf,
        may_reenter_javascript: !leaf,
        safepoint_required: !leaf,
    }
}

const fn baseline_support(op: Op) -> TierSupport {
    match op {
        Op::Nop
        | Op::LoadUndefined
        | Op::LoadTrue
        | Op::LoadFalse
        | Op::LoadNull
        | Op::LoadString
        | Op::LoadNumber
        | Op::LoadInt32
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Neg
        | Op::Equal
        | Op::NotEqual
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq
        | Op::Jump
        | Op::JumpIfTrue
        | Op::JumpIfFalse
        | Op::JumpIfNullish
        | Op::LoadLocal
        | Op::StoreLocal
        | Op::LoadProperty
        | Op::StoreProperty
        | Op::LoadElement
        | Op::StoreElement
        | Op::ArrayLength
        | Op::Call
        | Op::CallWithThis
        | Op::CallMethodValue
        | Op::Return
        | Op::ReturnValue
        | Op::ReturnUndefined => TierSupport::Partial,
        _ => TierSupport::Fallback,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn schema_is_dense_unique_and_matches_compatibility_table() {
        assert_eq!(OPCODE_SCHEMA.len(), OP_BYTE_TABLE.len());
        let mut ops = HashSet::new();
        let mut bytes = HashSet::new();
        for (index, (schema, compatibility)) in OPCODE_SCHEMA.iter().zip(OP_BYTE_TABLE).enumerate()
        {
            assert_eq!(schema.op, compatibility.0);
            assert_eq!(schema.byte, compatibility.1);
            assert_eq!(schema.byte as usize, index);
            assert!(
                ops.insert(schema.op),
                "duplicate schema row for {:?}",
                schema.op
            );
            assert!(
                bytes.insert(schema.byte),
                "duplicate schema byte 0x{:02X}",
                schema.byte
            );
            assert_eq!(opcode_schema(schema.op), schema);
        }
    }

    #[test]
    fn conservative_effects_require_safepoints() {
        for schema in OPCODE_SCHEMA {
            if schema.effects.may_allocate
                || schema.effects.may_trigger_gc
                || schema.effects.may_reenter_javascript
            {
                assert!(
                    schema.effects.safepoint_required,
                    "{:?} has a slow effect without a safepoint",
                    schema.op
                );
            }
        }
    }

    #[test]
    fn exact_shapes_drive_operand_count_and_register_sources() {
        for schema in OPCODE_SCHEMA {
            let Some(operands) = schema.operand_shape.prefix() else {
                continue;
            };
            assert_eq!(schema.op.operand_count(), operands.len(), "{:?}", schema.op);
            for spec in operands {
                assert_eq!(
                    spec.register_access == RegisterAccess::None,
                    spec.register_source.is_none(),
                    "{:?} has inconsistent register metadata",
                    schema.op
                );
            }
            if let Some((count_operand_index, tail)) = schema.operand_shape.variadic() {
                assert_eq!(operands[count_operand_index].kind, OperandKind::ConstIndex);
                assert_eq!(
                    tail.register_access == RegisterAccess::None,
                    tail.register_source.is_none(),
                    "{:?} has inconsistent variadic register metadata",
                    schema.op
                );
            }
        }
    }

    #[test]
    fn exact_relative_successors_reference_imm32_operands() {
        for schema in OPCODE_SCHEMA {
            let successors = schema.successor_shape.exact();
            for successor in successors {
                let SuccessorSpec::RelativeTarget { operand_index, .. } = successor else {
                    continue;
                };
                let operands = schema
                    .operand_shape
                    .fixed()
                    .expect("exact relative successors require an exact operand shape");
                assert_eq!(operands[*operand_index].kind, OperandKind::Imm32);
            }
            for successor in schema.exception_successor_shape.exact() {
                let ExceptionSuccessorSpec::OptionalRelativeTarget { operand_index, .. } =
                    successor
                else {
                    continue;
                };
                let operands = schema
                    .operand_shape
                    .fixed()
                    .expect("encoded exception targets require an exact fixed shape");
                assert_eq!(operands[*operand_index].kind, OperandKind::Imm32);
            }
        }
    }
}
