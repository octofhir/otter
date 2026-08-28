//! Declarative metadata schema for the active bytecode opcode set.
//!
//! # Contents
//! - [`OPCODE_SCHEMA`] is the dense, generated metadata table.
//! - [`OP_BYTE_TABLE`] is the compatibility view consumed by the current wire
//!   encoder and decoder.
//! - [`opcode_schema`] provides an exhaustive `Op` lookup.
//! - [`encode_operand_word`], [`decode_operand_word`], and
//!   [`operand_kind_at`] define schema-typed CodeBlock word operands.
//!
//! # Invariants
//! - One macro invocation owns opcode identity and byte assignment; generated
//!   compatibility views cannot drift from it.
//! - The serialized compiler/debug format remains self-describing while active
//!   CodeBlocks store untagged operand words whose kinds come only from this
//!   schema. Fixed and variadic families have exact operand/register roles.
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
    /// The current activation has already completed: route a catchable failure
    /// through the caller's handler or let it escape the dispatch stack.
    ///
    /// The return family owns this terminal intra-function edge because
    /// derived-constructor validation and async completion settlement happen
    /// after the returning frame is removed. `TailCall` deliberately retains
    /// [`Self::DynamicFrameHandlerOrCaller`]: an activation that cannot be
    /// discarded falls back to an ordinary call while its handlers remain
    /// active; a discarded activation is proven to own no handlers.
    CallerHandlerOrUncaught,
    /// Resume a parked throw/return/break/continue completion.
    ResumeParkedAbruptCompletion,
    /// Resume a suspended frame with a `return` completion: discard catch-only
    /// handlers, run every pending `finally`, then complete the frame.
    ///
    /// Ordinary `yield` owns this dynamic edge because
    /// `Generator.prototype.return` resumes after the suspension without
    /// executing another bytecode opcode first. Delegating `yield*` instead
    /// receives the resume kind as ordinary data.
    RunFinallyHandlersToFrameReturn,
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
    /// Global, captured, or dynamic-environment binding feedback.
    Binding,
}

/// Missing-binding behavior for one typed binding read.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BindingMissing {
    /// An unresolved reference raises `ReferenceError`.
    Throw,
    /// An unresolved reference produces `undefined` (`typeof` semantics).
    Undefined,
}

/// Typed read semantics and their authoritative operand roles.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BindingRead {
    /// Read the realm's stable `globalThis` value.
    GlobalThis {
        /// Result-register operand position.
        destination: u8,
    },
    /// Read through the global Environment Record.
    Global {
        /// Result-register operand position.
        destination: u8,
        /// String-constant operand position.
        name: u8,
        /// Unresolved-reference behavior.
        missing: BindingMissing,
    },
    /// Test whether the global Environment Record currently has a binding.
    Exists {
        /// Boolean result-register operand position.
        destination: u8,
        /// String-constant operand position.
        name: u8,
    },
    /// Read one captured cell, including its TDZ check.
    Upvalue {
        /// Result-register operand position.
        destination: u8,
        /// Upvalue-index operand position.
        index: u8,
    },
    /// Read the eval chain, then the global Environment Record.
    Dynamic {
        /// Result-register operand position.
        destination: u8,
        /// String-constant operand position.
        name: u8,
        /// Unresolved-reference behavior.
        missing: BindingMissing,
    },
    /// Read the eval chain, then one captured cell.
    ShadowedUpvalue {
        /// Result-register operand position.
        destination: u8,
        /// String-constant operand position.
        name: u8,
        /// Captured-cell index operand position.
        index: u8,
        /// Number of physical eval-environment records that may shadow the
        /// captured declaration.
        eval_depth: u8,
    },
}

/// Whether an upvalue store initializes a fresh cell or assigns a live binding.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BindingWriteCheck {
    /// Binding initialization may replace the TDZ hole.
    Initialize,
    /// Assignment must reject a TDZ hole.
    Checked,
}

/// Captured-binding behavior when a shadowed write finds no eval-chain cell.
///
/// The physical [`crate::Op::StoreShadowedUpvalueChecked`] site carries this
/// because an [`otter_vm::UpvalueCell`](https://docs.rs/otter-vm) stores only
/// the moving value, not the source binding's mutability. The eval-chain hit
/// always writes; this alphabet governs only the captured fallback.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[repr(i32)]
#[serde(rename_all = "kebab-case")]
pub enum ShadowedUpvalueFallback {
    /// Assignment-check the captured cell and write it.
    Mutable = 0,
    /// Reject the captured fallback as an immutable binding.
    ImmutableThrow = 1,
    /// Silently retain an immutable named-function self binding.
    ImmutableIgnore = 2,
}

impl ShadowedUpvalueFallback {
    /// Decode the complete schema-owned immediate alphabet.
    #[must_use]
    pub const fn from_imm32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Mutable),
            1 => Some(Self::ImmutableThrow),
            2 => Some(Self::ImmutableIgnore),
            _ => None,
        }
    }
}

/// One packed store policy for a shadowed captured binding.
///
/// The fixed-width instruction record has four inline operands. Packing the
/// positive physical eval depth with the three-value fallback alphabet keeps
/// `StoreShadowedUpvalueChecked` inline while this schema remains the sole
/// encoder/decoder authority.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ShadowedUpvalueStorePolicy {
    /// Number of physical eval-environment records permitted for lookup.
    pub eval_depth: u32,
    /// Captured-cell behavior when the bounded prefix has no matching name.
    pub fallback: ShadowedUpvalueFallback,
}

impl ShadowedUpvalueStorePolicy {
    const FALLBACK_CARDINALITY: u32 = 3;

    /// Encode a positive depth and fallback into the schema-owned immediate.
    #[must_use]
    pub const fn to_imm32(self) -> Option<i32> {
        if self.eval_depth == 0 {
            return None;
        }
        let Some(depth) = self.eval_depth.checked_mul(Self::FALLBACK_CARDINALITY) else {
            return None;
        };
        let Some(encoded) = depth.checked_add(self.fallback as u32) else {
            return None;
        };
        if encoded > i32::MAX as u32 {
            return None;
        }
        Some(encoded as i32)
    }

    /// Decode the complete packed policy domain.
    #[must_use]
    pub const fn from_imm32(value: i32) -> Option<Self> {
        if value < Self::FALLBACK_CARDINALITY as i32 {
            return None;
        }
        let encoded = value as u32;
        let eval_depth = encoded / Self::FALLBACK_CARDINALITY;
        let fallback = match encoded % Self::FALLBACK_CARDINALITY {
            0 => ShadowedUpvalueFallback::Mutable,
            1 => ShadowedUpvalueFallback::ImmutableThrow,
            2 => ShadowedUpvalueFallback::ImmutableIgnore,
            _ => return None,
        };
        Some(Self {
            eval_depth,
            fallback,
        })
    }
}

/// Typed write semantics and their authoritative operand roles.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BindingWrite {
    /// Write through the global Environment Record.
    Global {
        /// Boxed source-register operand position.
        value: u8,
        /// String-constant operand position.
        name: u8,
        /// Strictness-immediate operand position.
        strict: u8,
    },
    /// Strict global write guarded by a pre-RHS existence Boolean.
    GlobalChecked {
        /// Boxed source-register operand position.
        value: u8,
        /// String-constant operand position.
        name: u8,
        /// Pre-RHS existence-register operand position.
        exists: u8,
    },
    /// Write one captured cell.
    Upvalue {
        /// Boxed source-register operand position.
        value: u8,
        /// Upvalue-index operand position.
        index: u8,
        /// Initialization versus assignment semantics.
        check: BindingWriteCheck,
    },
    /// Write the eval chain, then the sloppy global Environment Record.
    Dynamic {
        /// Boxed source-register operand position.
        value: u8,
        /// String-constant operand position.
        name: u8,
        /// Strictness-immediate operand position for the global fallback.
        strict: u8,
    },
    /// Write the eval chain, then an assignment-checked captured cell.
    ShadowedUpvalue {
        /// Boxed source-register operand position.
        value: u8,
        /// String-constant operand position.
        name: u8,
        /// Captured-cell index operand position.
        index: u8,
        /// Packed [`ShadowedUpvalueStorePolicy`] immediate operand position.
        policy: u8,
    },
}

/// Typed binding deletion semantics and authoritative operand roles.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BindingDelete {
    /// Delete an eval-chain binding, otherwise a global binding.
    Dynamic {
        /// Boolean result-register operand position.
        destination: u8,
        /// String-constant operand position.
        name: u8,
    },
    /// Delete an eval-chain shadow, otherwise retain the captured binding.
    ShadowedUpvalue {
        /// Boolean result-register operand position.
        destination: u8,
        /// String-constant operand position.
        name: u8,
        /// Captured-cell index operand position used for structural validation.
        index: u8,
        /// Number of physical eval-environment records in which deletion is
        /// permitted.
        eval_depth: u8,
    },
}

/// Single semantic authority for all bytecode binding accesses.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "access", content = "semantics")]
pub enum BindingSemantics {
    /// A result-producing binding read.
    Read(BindingRead),
    /// A binding mutation.
    Write(BindingWrite),
    /// A result-producing binding deletion.
    Delete(BindingDelete),
}

impl BindingSemantics {
    /// Result-register operand, when the operation produces a value.
    #[must_use]
    pub const fn result_operand(self) -> Option<u8> {
        match self {
            Self::Read(BindingRead::GlobalThis { destination })
            | Self::Read(BindingRead::Global { destination, .. })
            | Self::Read(BindingRead::Exists { destination, .. })
            | Self::Read(BindingRead::Upvalue { destination, .. })
            | Self::Read(BindingRead::Dynamic { destination, .. })
            | Self::Read(BindingRead::ShadowedUpvalue { destination, .. })
            | Self::Delete(BindingDelete::Dynamic { destination, .. })
            | Self::Delete(BindingDelete::ShadowedUpvalue { destination, .. }) => Some(destination),
            Self::Write(_) => None,
        }
    }

    /// Boxed SSA input-register operands in ABI order.
    #[must_use]
    pub const fn value_operands(self) -> [Option<u8>; 2] {
        match self {
            Self::Write(BindingWrite::Global { value, .. })
            | Self::Write(BindingWrite::Upvalue { value, .. })
            | Self::Write(BindingWrite::Dynamic { value, .. })
            | Self::Write(BindingWrite::ShadowedUpvalue { value, .. }) => [Some(value), None],
            Self::Write(BindingWrite::GlobalChecked { value, exists, .. }) => {
                [Some(value), Some(exists)]
            }
            Self::Read(_) | Self::Delete(_) => [None, None],
        }
    }
}

/// Separate global declaration/initialization family.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum GlobalDeclarationSemantics {
    /// CreateGlobalVarBinding.
    DeclareVar {
        /// String-constant operand position.
        name: u8,
        /// Configurable-flag immediate operand position.
        configurable: u8,
    },
    /// CreateMutableBinding/CreateImmutableBinding.
    DeclareLexical {
        /// String-constant operand position.
        name: u8,
        /// Constness immediate operand position.
        is_const: u8,
    },
    /// Preflight one GlobalDeclarationInstantiation name.
    Validate {
        /// String-constant operand position.
        name: u8,
        /// Declaration-kind immediate operand position.
        declaration_kind: u8,
    },
    /// Initialize/update one global var binding.
    DefineVar {
        /// String-constant operand position.
        name: u8,
        /// Boxed source-register operand position.
        value: u8,
    },
    /// CreateGlobalFunctionBinding.
    DefineFunction {
        /// String-constant operand position.
        name: u8,
        /// Boxed function-value register operand position.
        value: u8,
        /// Deletable-flag immediate operand position.
        deletable: u8,
    },
    /// Initialize one global lexical cell.
    InitializeLexical {
        /// Boxed initializer-register operand position.
        value: u8,
        /// String-constant operand position.
        name: u8,
    },
}

impl GlobalDeclarationSemantics {
    /// Declaration operations do not produce a JavaScript result register.
    #[must_use]
    pub const fn result_operand(self) -> Option<u8> {
        None
    }

    /// Boxed SSA input-register operands in ABI order.
    #[must_use]
    pub const fn value_operands(self) -> [Option<u8>; 2] {
        match self {
            Self::DefineVar { value, .. }
            | Self::DefineFunction { value, .. }
            | Self::InitializeLexical { value, .. } => [Some(value), None],
            Self::DeclareVar { .. } | Self::DeclareLexical { .. } | Self::Validate { .. } => {
                [None, None]
            }
        }
    }
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
    /// Typed binding semantics, when this is a binding access.
    pub binding: Option<BindingSemantics>,
    /// Typed global declaration/initialization semantics.
    pub global_declaration: Option<GlobalDeclarationSemantics>,
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
            feedback: match binding_semantics(op) {
                Some(_) => FeedbackKind::Binding,
                None => feedback(op),
            },
            binding: binding_semantics(op),
            global_declaration: global_declaration_semantics(op),
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
    (Op::TailCall, 0xA6),
    (Op::IsEvalIntrinsic, 0xA7),
    (Op::PopParkedFinally, 0xA8),
    (Op::GlobalBindingExists, 0xA9),
    (Op::StoreGlobalChecked, 0xAA),
    (Op::AddImm, 0xAB),
    (Op::SubImm, 0xAC),
    (Op::BitwiseAndImm, 0xAD),
    (Op::LessThanImm, 0xAE),
    (Op::EqualImm, 0xAF),
    (Op::NotEqualImm, 0xB0),
    (Op::SuperConstruct, 0xB1),
    (Op::StoreShadowedUpvalueChecked, 0xB2),
    (Op::DeleteShadowedUpvalue, 0xB3),
    (Op::AsyncIteratorReturn, 0xB4),
    (Op::CheckIteratorResult, 0xB5),
    (Op::EvalBindingSeq, 0xB6),
    (Op::LoadShadowedUpvalueSnap, 0xB7),
    (Op::StoreShadowedUpvalueCheckedSnap, 0xB8),
    (Op::EvalRestoreBinding, 0xB9),
    (Op::StorePropertyStrict, 0xBA),
    (Op::StoreElementStrict, 0xBB),
}

/// Return the authoritative schema row for `op`.
#[must_use]
pub const fn opcode_schema(op: Op) -> &'static OpcodeSchema {
    &OPCODE_SCHEMA[schema_index(op)]
}

/// Return the schema-declared kind at one fixed or variadic operand position.
#[must_use]
pub const fn operand_kind_at(op: Op, index: usize) -> Option<OperandKind> {
    match operand_spec_at(op, index) {
        Some(spec) => Some(spec.kind),
        None => None,
    }
}

/// Return the authoritative operand kind and register role at one position.
///
/// Variadic positions after the fixed prefix use the declared homogeneous
/// tail. The decoded instruction remains responsible for bounding `index` by
/// its actual operand count.
#[must_use]
pub const fn operand_spec_at(op: Op, index: usize) -> Option<OperandSpec> {
    let shape = opcode_schema(op).operand_shape;
    let Some(prefix) = shape.prefix() else {
        return None;
    };
    if index < prefix.len() {
        return Some(prefix[index]);
    }
    match shape.variadic() {
        Some((_, tail)) => Some(tail),
        None => None,
    }
}

/// Return the schema-declared register data-flow role at one operand position.
///
/// `RegisterAccess::None` means the operand does not address the executing
/// frame's register window. Anything else does, whether it is spelled as a
/// `Register` operand or as the `Imm32` local index of `LoadLocal` /
/// `StoreLocal` — a verifier must bound-check both against the frame's
/// register count, and this is the single declaration of which those are.
#[must_use]
pub const fn register_access_at(op: Op, index: usize) -> RegisterAccess {
    match operand_spec_at(op, index) {
        Some(spec) => spec.register_access,
        None => RegisterAccess::None,
    }
}

/// Encode one verified operand as an untagged 32-bit CodeBlock word.
#[must_use]
pub const fn encode_operand_word(operand: Operand) -> u32 {
    match operand {
        Operand::Register(value) => value as u32,
        Operand::ConstIndex(value) => value,
        Operand::Imm32(value) => value as u32,
    }
}

/// Decode one CodeBlock word using its authoritative schema kind.
#[must_use]
pub fn decode_operand_word(kind: OperandKind, word: u32) -> Option<Operand> {
    Some(match kind {
        OperandKind::Register => Operand::Register(u16::try_from(word).ok()?),
        OperandKind::ConstIndex => Operand::ConstIndex(word),
        OperandKind::Imm32 => Operand::Imm32(word as i32),
    })
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
const READ_READ_READ: &[OperandSpec] = &[R, R, R];
const WRITE_WRITE_READ: &[OperandSpec] = &[W, W, R];
const WRITE_CONST_IMM_IMM: &[OperandSpec] = &[W, CONST, IMM, IMM];
const JUMP_VIA_FINALLY: &[OperandSpec] = &[IMM, IMM];
const READ_CONST: &[OperandSpec] = &[R, CONST];
const CONST_READ: &[OperandSpec] = &[CONST, R];
const WRITE_READ_WRITE: &[OperandSpec] = &[W, R, W];
const WRITE_READ_READ_READ: &[OperandSpec] = &[W, R, R, R];
const WRITE_FOUR_READS: &[OperandSpec] = &[W, R, R, R, R];
const WRITE_READ_IMM: &[OperandSpec] = &[W, R, IMM];
const WRITE_READ_IMM_IMM: &[OperandSpec] = &[W, R, IMM, IMM];
const CONST_READ_IMM: &[OperandSpec] = &[CONST, R, IMM];
const CONST_IMM: &[OperandSpec] = &[CONST, IMM];
const READ_CONST_IMM: &[OperandSpec] = &[R, CONST, IMM];
const READ_CONST_IMM_IMM: &[OperandSpec] = &[R, CONST, IMM, IMM];

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
        Op::New | Op::SuperConstruct => OperandShape::Variadic {
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
        Op::Throw => OperandShape::Fixed(&[R]),
        Op::EnterTry => OperandShape::Fixed(ENTER_TRY),
        Op::EndFinally => OperandShape::Fixed(EMPTY),
        Op::NewObject => OperandShape::Fixed(WRITE),
        Op::LoadProperty | Op::DeleteProperty => OperandShape::Fixed(WRITE_READ_CONST),
        Op::StoreProperty => OperandShape::Fixed(READ_CONST_READ_WRITE),
        Op::StorePropertyStrict => OperandShape::Fixed(READ_CONST_READ_WRITE),
        Op::GetPrototype | Op::ArrayLength | Op::GetIterator | Op::GetAsyncIterator => {
            OperandShape::Fixed(WRITE_READ)
        }
        Op::SetPrototype | Op::ArrayPush => OperandShape::Fixed(READ_READ),
        Op::CopyDataProperties => OperandShape::Fixed(READ_READ_READ),
        Op::LoadElement | Op::DeleteElement | Op::HasProperty | Op::Instanceof => {
            OperandShape::Fixed(WRITE_READ_READ)
        }
        Op::StoreElement => OperandShape::Fixed(READ_READ_READ),
        Op::StoreElementStrict => OperandShape::Fixed(READ_READ_READ),
        Op::IteratorNext => OperandShape::Fixed(WRITE_WRITE_READ),
        Op::IteratorClose
        | Op::IteratorCloseStart
        | Op::IteratorCloseEnd
        | Op::CheckIteratorResult => OperandShape::Fixed(&[R]),
        Op::AsyncIteratorReturn => OperandShape::Fixed(WRITE_WRITE_READ),
        Op::ForInKeys => OperandShape::Fixed(WRITE_READ),
        Op::LoadUpvalue => OperandShape::Fixed(WRITE_IMM),
        Op::StoreUpvalue | Op::StoreUpvalueChecked => OperandShape::Fixed(&[R, IMM]),
        Op::FreshUpvalue => OperandShape::Fixed(&[IMM]),
        Op::LoadShadowedUpvalue | Op::DeleteShadowedUpvalue => {
            OperandShape::Fixed(WRITE_CONST_IMM_IMM)
        }
        Op::StoreShadowedUpvalueChecked => OperandShape::Fixed(READ_CONST_IMM_IMM),
        Op::EvalBindingSeq => OperandShape::Fixed(WRITE),
        Op::LoadShadowedUpvalueSnap => OperandShape::Fixed(&[W, CONST, IMM, IMM, R]),
        Op::StoreShadowedUpvalueCheckedSnap => OperandShape::Fixed(&[R, CONST, IMM, IMM, R]),
        Op::EvalRestoreBinding => OperandShape::Fixed(CONST_IMM),
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
        | Op::PromiseFulfilledOf
        | Op::NewWeakRef
        | Op::NewFinalizationRegistry
        | Op::Yield => OperandShape::Fixed(WRITE_READ),
        Op::CallSpread => OperandShape::Fixed(WRITE_READ_READ_READ),
        Op::NewSpread | Op::SuperConstructSpread | Op::ImportNamespaceDynamic => {
            OperandShape::Fixed(WRITE_READ_READ)
        }
        Op::MakeClass => OperandShape::Fixed(WRITE_FOUR_READS),
        Op::CollectRest | Op::CollectArguments => OperandShape::Fixed(WRITE),
        Op::Increment => OperandShape::Fixed(WRITE_READ_IMM),
        Op::Eval => OperandShape::Fixed(WRITE_READ_IMM_IMM),
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
        Op::StoreDynamic => OperandShape::Fixed(READ_CONST_IMM),
        Op::InitGlobalLex => OperandShape::Fixed(READ_CONST),
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
        Op::AddImm
        | Op::SubImm
        | Op::BitwiseAndImm
        | Op::LessThanImm
        | Op::EqualImm
        | Op::NotEqualImm => OperandShape::Fixed(WRITE_READ_IMM),
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
const RETURN_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] =
    &[ExceptionSuccessorSpec::CallerHandlerOrUncaught];
const YIELD_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] = &[
    ExceptionSuccessorSpec::DynamicFrameHandlerOrCaller,
    ExceptionSuccessorSpec::RunFinallyHandlersToFrameReturn,
];
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
        Op::Return | Op::ReturnValue | Op::ReturnUndefined => {
            ExceptionSuccessorShape::new(RETURN_EXCEPTION_SUCCESSORS)
        }
        Op::Yield => ExceptionSuccessorShape::new(YIELD_EXCEPTION_SUCCESSORS),
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
        | Op::SuperConstruct
        | Op::SuperConstructSpread
        | Op::Eval
        | Op::PromiseCall => ControlFlow::Call,
        _ => ControlFlow::Fallthrough,
    }
}

const fn binding_semantics(op: Op) -> Option<BindingSemantics> {
    match op {
        Op::LoadGlobalThis => Some(BindingSemantics::Read(BindingRead::GlobalThis {
            destination: 0,
        })),
        Op::LoadGlobalOrThrow => Some(BindingSemantics::Read(BindingRead::Global {
            destination: 0,
            name: 1,
            missing: BindingMissing::Throw,
        })),
        Op::LoadGlobalOrUndefined => Some(BindingSemantics::Read(BindingRead::Global {
            destination: 0,
            name: 1,
            missing: BindingMissing::Undefined,
        })),
        Op::GlobalBindingExists => Some(BindingSemantics::Read(BindingRead::Exists {
            destination: 0,
            name: 1,
        })),
        Op::LoadUpvalue => Some(BindingSemantics::Read(BindingRead::Upvalue {
            destination: 0,
            index: 1,
        })),
        Op::LoadDynamic => Some(BindingSemantics::Read(BindingRead::Dynamic {
            destination: 0,
            name: 1,
            missing: BindingMissing::Throw,
        })),
        Op::TypeofDynamic => Some(BindingSemantics::Read(BindingRead::Dynamic {
            destination: 0,
            name: 1,
            missing: BindingMissing::Undefined,
        })),
        Op::LoadShadowedUpvalue => Some(BindingSemantics::Read(BindingRead::ShadowedUpvalue {
            destination: 0,
            name: 1,
            index: 2,
            eval_depth: 3,
        })),
        Op::StoreGlobalBinding => Some(BindingSemantics::Write(BindingWrite::Global {
            value: 0,
            name: 1,
            strict: 2,
        })),
        Op::StoreGlobalChecked => Some(BindingSemantics::Write(BindingWrite::GlobalChecked {
            value: 0,
            name: 1,
            exists: 2,
        })),
        Op::StoreUpvalue => Some(BindingSemantics::Write(BindingWrite::Upvalue {
            value: 0,
            index: 1,
            check: BindingWriteCheck::Initialize,
        })),
        Op::StoreUpvalueChecked => Some(BindingSemantics::Write(BindingWrite::Upvalue {
            value: 0,
            index: 1,
            check: BindingWriteCheck::Checked,
        })),
        Op::StoreDynamic => Some(BindingSemantics::Write(BindingWrite::Dynamic {
            value: 0,
            name: 1,
            strict: 2,
        })),
        Op::StoreShadowedUpvalueChecked => {
            Some(BindingSemantics::Write(BindingWrite::ShadowedUpvalue {
                value: 0,
                name: 1,
                index: 2,
                policy: 3,
            }))
        }
        Op::DeleteDynamic => Some(BindingSemantics::Delete(BindingDelete::Dynamic {
            destination: 0,
            name: 1,
        })),
        Op::DeleteShadowedUpvalue => {
            Some(BindingSemantics::Delete(BindingDelete::ShadowedUpvalue {
                destination: 0,
                name: 1,
                index: 2,
                eval_depth: 3,
            }))
        }
        _ => None,
    }
}

const fn global_declaration_semantics(op: Op) -> Option<GlobalDeclarationSemantics> {
    match op {
        Op::DeclareGlobalVar => Some(GlobalDeclarationSemantics::DeclareVar {
            name: 0,
            configurable: 1,
        }),
        Op::DeclareGlobalLex => Some(GlobalDeclarationSemantics::DeclareLexical {
            name: 0,
            is_const: 1,
        }),
        Op::ValidateGlobalDecl => Some(GlobalDeclarationSemantics::Validate {
            name: 0,
            declaration_kind: 1,
        }),
        Op::DefineGlobalVar => Some(GlobalDeclarationSemantics::DefineVar { name: 0, value: 1 }),
        Op::DefineGlobalFunction => Some(GlobalDeclarationSemantics::DefineFunction {
            name: 0,
            value: 1,
            deletable: 2,
        }),
        Op::InitGlobalLex => {
            Some(GlobalDeclarationSemantics::InitializeLexical { value: 0, name: 1 })
        }
        _ => None,
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
        Op::LoadProperty
        | Op::StoreProperty
        | Op::StorePropertyStrict
        | Op::HasProperty
        | Op::DeleteProperty => FeedbackKind::Property,
        Op::LoadElement
        | Op::StoreElement
        | Op::StoreElementStrict
        | Op::DeleteElement
        | Op::ArrayLength => FeedbackKind::Element,
        Op::Call
        | Op::CallWithThis
        | Op::CallMethodValue
        | Op::CallSpread
        | Op::TailCall
        | Op::New
        | Op::NewSpread
        | Op::SuperConstruct
        | Op::SuperConstructSpread => FeedbackKind::Call,
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
            | Op::LoadInt32
            | Op::LoadNumber
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
        // These remain allocation-free, non-reentrant leaf operations while
        // owning typed exception exits: `LoadThis` can hit the derived-`this`
        // TDZ, while return completion can fail only after leaving this frame.
        may_throw: !leaf
            || matches!(
                op,
                Op::LoadThis | Op::Return | Op::ReturnValue | Op::ReturnUndefined
            ),
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
    fn binding_feedback_covers_the_complete_access_family() {
        let expected = HashSet::from([
            Op::LoadGlobalThis,
            Op::LoadGlobalOrThrow,
            Op::LoadGlobalOrUndefined,
            Op::GlobalBindingExists,
            Op::StoreGlobalBinding,
            Op::StoreGlobalChecked,
            Op::LoadUpvalue,
            Op::StoreUpvalue,
            Op::StoreUpvalueChecked,
            Op::LoadDynamic,
            Op::StoreDynamic,
            Op::TypeofDynamic,
            Op::LoadShadowedUpvalue,
            Op::DeleteDynamic,
            Op::StoreShadowedUpvalueChecked,
            Op::DeleteShadowedUpvalue,
        ]);
        let actual = OPCODE_SCHEMA
            .iter()
            .filter_map(|schema| schema.binding.map(|_| schema.op))
            .collect::<HashSet<_>>();

        assert_eq!(actual, expected);
        for schema in OPCODE_SCHEMA {
            assert_eq!(
                schema.feedback == FeedbackKind::Binding,
                schema.binding.is_some(),
                "{:?} has divergent binding feedback/schema authority",
                schema.op
            );
        }
    }

    #[test]
    fn shadowed_store_policy_owns_the_complete_packed_domain() {
        for fallback in [
            ShadowedUpvalueFallback::Mutable,
            ShadowedUpvalueFallback::ImmutableThrow,
            ShadowedUpvalueFallback::ImmutableIgnore,
        ] {
            for eval_depth in [1, 2, 17, 1_000_000] {
                let policy = ShadowedUpvalueStorePolicy {
                    eval_depth,
                    fallback,
                };
                let encoded = policy.to_imm32().expect("representable policy");
                assert_eq!(
                    ShadowedUpvalueStorePolicy::from_imm32(encoded),
                    Some(policy)
                );
            }
        }
        assert_eq!(
            ShadowedUpvalueStorePolicy {
                eval_depth: 0,
                fallback: ShadowedUpvalueFallback::Mutable,
            }
            .to_imm32(),
            None
        );
        assert_eq!(ShadowedUpvalueStorePolicy::from_imm32(-1), None);
        assert_eq!(ShadowedUpvalueStorePolicy::from_imm32(0), None);
        assert_eq!(ShadowedUpvalueStorePolicy::from_imm32(1), None);
        assert_eq!(ShadowedUpvalueStorePolicy::from_imm32(2), None);
        assert_eq!(
            ShadowedUpvalueStorePolicy {
                eval_depth: u32::MAX,
                fallback: ShadowedUpvalueFallback::ImmutableIgnore,
            }
            .to_imm32(),
            None
        );
    }

    #[test]
    fn binding_semantic_operands_agree_with_wire_roles() {
        let assert_operand =
            |schema: &OpcodeSchema, index: u8, kind: OperandKind, access: RegisterAccess| {
                let actual = operand_spec_at(schema.op, usize::from(index));
                assert_eq!(
                    actual,
                    Some(OperandSpec {
                        kind,
                        register_access: access,
                        register_source: (access != RegisterAccess::None)
                            .then_some(RegisterSource::RegisterOperand),
                    }),
                    "{:?} operand {index}",
                    schema.op
                );
            };
        for schema in OPCODE_SCHEMA {
            let Some(binding) = schema.binding else {
                continue;
            };
            if let Some(result) = binding.result_operand() {
                assert_operand(schema, result, OperandKind::Register, RegisterAccess::Write);
            }
            for value in binding.value_operands().into_iter().flatten() {
                assert_operand(schema, value, OperandKind::Register, RegisterAccess::Read);
            }
            match binding {
                BindingSemantics::Read(BindingRead::GlobalThis { .. }) => {}
                BindingSemantics::Read(BindingRead::Upvalue { index, .. }) => {
                    assert_operand(schema, index, OperandKind::Imm32, RegisterAccess::None);
                }
                BindingSemantics::Read(BindingRead::Global { name, .. })
                | BindingSemantics::Read(BindingRead::Exists { name, .. })
                | BindingSemantics::Read(BindingRead::Dynamic { name, .. })
                | BindingSemantics::Delete(BindingDelete::Dynamic { name, .. }) => {
                    assert_operand(schema, name, OperandKind::ConstIndex, RegisterAccess::None);
                }
                BindingSemantics::Read(BindingRead::ShadowedUpvalue {
                    name,
                    index,
                    eval_depth,
                    ..
                })
                | BindingSemantics::Delete(BindingDelete::ShadowedUpvalue {
                    name,
                    index,
                    eval_depth,
                    ..
                }) => {
                    assert_operand(schema, name, OperandKind::ConstIndex, RegisterAccess::None);
                    assert_operand(schema, index, OperandKind::Imm32, RegisterAccess::None);
                    assert_operand(schema, eval_depth, OperandKind::Imm32, RegisterAccess::None);
                }
                BindingSemantics::Write(BindingWrite::Global { name, strict, .. }) => {
                    assert_operand(schema, name, OperandKind::ConstIndex, RegisterAccess::None);
                    assert_operand(schema, strict, OperandKind::Imm32, RegisterAccess::None);
                }
                BindingSemantics::Write(BindingWrite::GlobalChecked { name, .. }) => {
                    assert_operand(schema, name, OperandKind::ConstIndex, RegisterAccess::None);
                }
                BindingSemantics::Write(BindingWrite::Dynamic { name, strict, .. }) => {
                    assert_operand(schema, name, OperandKind::ConstIndex, RegisterAccess::None);
                    assert_operand(schema, strict, OperandKind::Imm32, RegisterAccess::None);
                }
                BindingSemantics::Write(BindingWrite::Upvalue { index, .. }) => {
                    assert_operand(schema, index, OperandKind::Imm32, RegisterAccess::None);
                }
                BindingSemantics::Write(BindingWrite::ShadowedUpvalue {
                    name,
                    index,
                    policy,
                    ..
                }) => {
                    assert_operand(schema, name, OperandKind::ConstIndex, RegisterAccess::None);
                    assert_operand(schema, index, OperandKind::Imm32, RegisterAccess::None);
                    assert_operand(schema, policy, OperandKind::Imm32, RegisterAccess::None);
                }
            }
        }
    }

    #[test]
    fn global_declaration_family_is_disjoint_from_binding_accesses() {
        let expected = HashSet::from([
            Op::DeclareGlobalVar,
            Op::DeclareGlobalLex,
            Op::ValidateGlobalDecl,
            Op::DefineGlobalVar,
            Op::DefineGlobalFunction,
            Op::InitGlobalLex,
        ]);
        let actual = OPCODE_SCHEMA
            .iter()
            .filter_map(|schema| schema.global_declaration.map(|_| schema.op))
            .collect::<HashSet<_>>();
        assert_eq!(actual, expected);
        assert!(actual.iter().all(|op| opcode_schema(*op).binding.is_none()));
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
    fn word_operands_round_trip_through_schema_kinds() {
        let cases = [
            (OperandKind::Register, Operand::Register(u16::MAX)),
            (OperandKind::ConstIndex, Operand::ConstIndex(u32::MAX)),
            (OperandKind::Imm32, Operand::Imm32(i32::MIN)),
        ];
        for (kind, operand) in cases {
            assert_eq!(
                decode_operand_word(kind, encode_operand_word(operand)),
                Some(operand)
            );
        }
        assert_eq!(
            operand_kind_at(Op::MakeClass, 4),
            Some(OperandKind::Register)
        );
        assert_eq!(operand_kind_at(Op::MakeClass, 5), None);
        assert_eq!(operand_kind_at(Op::Call, 4), Some(OperandKind::Register));
        assert_eq!(
            operand_spec_at(Op::Call, 4),
            Some(OperandSpec::register(RegisterAccess::Read))
        );
        assert_eq!(
            operand_spec_at(Op::LoadLocal, 1),
            Some(OperandSpec::local_index(RegisterAccess::Read))
        );
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
