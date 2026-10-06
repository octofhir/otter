//! Declarative metadata schema for the active bytecode opcode set.
//!
//! # Contents
//! - [`OPCODE_SCHEMA`] is the dense, generated metadata table.
//! - [`OP_BYTE_TABLE`] is the compatibility view consumed by the current wire
//!   encoder and decoder.
//! - [`opcode_schema`] provides an exhaustive `Op` lookup.
//! - [`encode_operand_word`], [`decode_operand_word`], and
//!   [`operand_kind_at`] define schema-typed CodeBlock word operands.
//! - [`ImmediateDomain`] types the packed immediates of the context and
//!   lookup families: [`ContextCoord`], [`LookupRefTarget`],
//!   [`LookupGlobalMode`], [`StoreRefMode`], and [`BindingStoreFallback`].
//! - [`BindingSemantics`] is the single semantic authority for binding
//!   accesses that may throw or consult eval extensions.
//!
//! # Invariants
//! - One macro invocation owns opcode identity and byte assignment; generated
//!   compatibility views cannot drift from it.
//! - The serialized compiler/debug format remains self-describing while active
//!   CodeBlocks store untagged operand words whose kinds come only from this
//!   schema. Fixed and variadic families have exact operand/register roles.
//! - Every packed immediate has exactly one encoder and one decoder, here;
//!   the verifier admits an operand only when its domain decodes it.
//! - Effects are exact for register/frame leaves and for the context-slot
//!   family, and conservative (everything possible) for every other opcode.
//!   A leaf never claims to allocate, trigger GC, re-enter JavaScript, or
//!   require a safepoint.
//! - Only checked, lookup, and global binding accesses carry
//!   [`BindingSemantics`]. Unchecked context-slot loads and stores are plain
//!   memory operations with no binding row and no exception edge.
//!
//! # See also
//! - [`crate::encoding`] for the unchanged executable byte format.
//! - [`crate::opcode_audit`] for the machine-readable schema projection.
//! - [`crate::ScopeDescriptor`] for the scopes a [`ContextCoord`] addresses.

use serde::Serialize;

use crate::{Op, Operand};

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

/// Typed meaning of one `Imm32` operand of the context and lookup families.
///
/// The verifier admits the operand only when its domain decodes it; the
/// disassembler renders it through the same decoder.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ImmediateDomain {
    /// Index into the executing function's [`crate::Function::scopes`].
    ScopeIndex,
    /// A packed [`ContextCoord`] naming one slot.
    ContextCoord,
    /// A context-chain hop count in `0..=u16::MAX`.
    ContextDepth,
    /// A packed [`LookupRefTarget`].
    LookupRefTarget,
    /// A packed [`LookupGlobalMode`].
    LookupGlobalMode,
    /// A [`BindingStoreFallback`].
    StoreFallback,
    /// A packed [`StoreRefMode`].
    StoreRefMode,
    /// A packed [`MemberDefinition`].
    MemberDefinition,
}

impl ImmediateDomain {
    /// Whether `value` decodes in this domain. [`Self::ScopeIndex`] admits
    /// every non-negative value here; its upper bound is the owning
    /// function's scope table, which only the verifier sees.
    #[must_use]
    pub const fn admits(self, value: i32) -> bool {
        match self {
            Self::ScopeIndex => value >= 0,
            Self::ContextCoord => ContextCoord::from_imm32(value).is_some(),
            Self::ContextDepth => value >= 0 && value <= u16::MAX as i32,
            Self::LookupRefTarget => true,
            Self::LookupGlobalMode => LookupGlobalMode::from_imm32(value).is_some(),
            Self::StoreFallback => BindingStoreFallback::from_imm32(value).is_some(),
            Self::StoreRefMode => StoreRefMode::from_imm32(value).is_some(),
            Self::MemberDefinition => MemberDefinition::from_imm32(value).is_some(),
        }
    }
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
    /// Typed domain of a packed `Imm32` operand, when the schema owns one.
    pub imm_domain: Option<ImmediateDomain>,
}

impl OperandSpec {
    const fn value(kind: OperandKind) -> Self {
        Self {
            kind,
            register_access: RegisterAccess::None,
            register_source: None,
            imm_domain: None,
        }
    }

    const fn immediate(domain: ImmediateDomain) -> Self {
        Self {
            kind: OperandKind::Imm32,
            register_access: RegisterAccess::None,
            register_source: None,
            imm_domain: Some(domain),
        }
    }

    const fn register(access: RegisterAccess) -> Self {
        Self {
            kind: OperandKind::Register,
            register_access: access,
            register_source: Some(RegisterSource::RegisterOperand),
            imm_domain: None,
        }
    }

    const fn local_index(access: RegisterAccess) -> Self {
        Self {
            kind: OperandKind::Imm32,
            register_access: access,
            register_source: Some(RegisterSource::Imm32RegisterIndex),
            imm_domain: None,
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
    /// Land in the innermost entry of the function's handler table that
    /// covers the instruction, or continue in the caller.
    HandlerTableOrCaller,
    /// The current activation has already completed: route a catchable failure
    /// through the caller's handler or let it escape the dispatch stack.
    ///
    /// The return family owns this terminal intra-function edge because
    /// derived-constructor validation and async completion settlement happen
    /// after the returning frame is removed.
    CallerHandlerOrUncaught,
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

/// Packed context-chain coordinate: `depth << 16 | slot`.
///
/// `depth` counts parent links from the context a register names; `slot`
/// indexes the target context's slots in its scope descriptor. Slot
/// [`Self::RESERVED_SLOT`] marks the global forms of [`LookupRefTarget`] and
/// [`StoreRefMode`], so one context holds at most `MAX_SLOT + 1` slots and a
/// larger scope is a compile error.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
pub struct ContextCoord {
    /// Parent links to follow from the named context.
    pub depth: u16,
    /// Slot index in the target context.
    pub slot: u16,
}

impl ContextCoord {
    /// Slot value that never names a slot.
    pub const RESERVED_SLOT: u16 = u16::MAX;
    /// Largest addressable slot index.
    pub const MAX_SLOT: u16 = u16::MAX - 1;

    /// A coordinate, or `None` for the reserved slot.
    #[must_use]
    pub const fn new(depth: u16, slot: u16) -> Option<Self> {
        if slot == Self::RESERVED_SLOT {
            return None;
        }
        Some(Self { depth, slot })
    }

    /// Encode as `depth << 16 | slot`.
    #[must_use]
    pub const fn to_imm32(self) -> i32 {
        debug_assert!(self.slot != Self::RESERVED_SLOT);
        (((self.depth as u32) << 16) | self.slot as u32) as i32
    }

    /// Decode a packed coordinate; the reserved slot is not a coordinate.
    #[must_use]
    pub const fn from_imm32(value: i32) -> Option<Self> {
        let bits = value as u32;
        Self::new((bits >> 16) as u16, bits as u16)
    }
}

/// Target of a [`crate::Op::ResolveLookupRef`]: where the reference
/// resolves when no eval extension holds the name.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum LookupRefTarget {
    /// A context-slot binding; extensions at hops `[0, depth)` are probed
    /// first.
    Slot(ContextCoord),
    /// No static binding; extensions at hops `[0, depth)` are probed, then
    /// the global Environment Record.
    Global {
        /// Hop count bounding the extension probe.
        depth: u16,
    },
}

impl LookupRefTarget {
    /// Hop count bounding the extension probe.
    #[must_use]
    pub const fn depth(self) -> u16 {
        match self {
            Self::Slot(coord) => coord.depth,
            Self::Global { depth } => depth,
        }
    }

    /// Encode as a [`ContextCoord`] word; the global form carries
    /// [`ContextCoord::RESERVED_SLOT`] in the slot half.
    #[must_use]
    pub const fn to_imm32(self) -> i32 {
        match self {
            Self::Slot(coord) => coord.to_imm32(),
            Self::Global { depth } => {
                (((depth as u32) << 16) | ContextCoord::RESERVED_SLOT as u32) as i32
            }
        }
    }

    /// Decode; every 32-bit word is a target.
    #[must_use]
    pub const fn from_imm32(value: i32) -> Self {
        match ContextCoord::from_imm32(value) {
            Some(coord) => Self::Slot(coord),
            None => Self::Global {
                depth: ((value as u32) >> 16) as u16,
            },
        }
    }
}

/// Store behavior when a lookup store lands on a context slot rather than an
/// eval-extension entry.
///
/// A context slot holds only the moving value, not its binding's
/// mutability, so the store site carries it (§9.1.1.1.5 SetMutableBinding).
/// An extension entry is always mutable and always written.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[repr(i32)]
#[serde(rename_all = "kebab-case")]
pub enum BindingStoreFallback {
    /// Assignment to a mutable binding: a TDZ hole raises `ReferenceError`,
    /// otherwise the slot is written.
    Mutable = 0,
    /// Assignment to an immutable binding raises `TypeError`.
    ImmutableThrow = 1,
    /// Assignment to a sloppy function self-name is silently dropped.
    ImmutableIgnore = 2,
}

impl BindingStoreFallback {
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

    /// Encode as the schema-owned immediate.
    #[must_use]
    pub const fn to_imm32(self) -> i32 {
        self as i32
    }
}

/// Packed immediate of [`crate::Op::StoreLookupGlobal`]:
/// `depth | strict << 31`, bits 16–30 zero.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
pub struct LookupGlobalMode {
    /// Hop count bounding the extension probe.
    pub depth: u16,
    /// Strictness of the global `SetMutableBinding` fallback.
    pub strict: bool,
}

impl LookupGlobalMode {
    const STRICT_BIT: u32 = 1 << 31;
    const DEPTH_BITS: u32 = u16::MAX as u32;

    /// Encode as `depth | strict << 31`.
    #[must_use]
    pub const fn to_imm32(self) -> i32 {
        let strict = if self.strict { Self::STRICT_BIT } else { 0 };
        (self.depth as u32 | strict) as i32
    }

    /// Decode, rejecting any set reserved bit.
    #[must_use]
    pub const fn from_imm32(value: i32) -> Option<Self> {
        let bits = value as u32;
        if bits & !(Self::DEPTH_BITS | Self::STRICT_BIT) != 0 {
            return None;
        }
        Some(Self {
            depth: (bits & Self::DEPTH_BITS) as u16,
            strict: bits & Self::STRICT_BIT != 0,
        })
    }
}

/// What [`crate::Op::DefineMember`] installs.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
pub enum MemberKind {
    /// A data property: a method or a field value.
    Value = 0,
    /// The `[[Get]]` half of an accessor property.
    Getter = 1,
    /// The `[[Set]]` half of an accessor property.
    Setter = 2,
}

/// Packed immediate of [`crate::Op::DefineMember`].
///
/// Bit layout: bits 0–1 [`MemberKind`], bit 2 enumerable, bit 3 read-only
/// (a value only: a private method is not writable), bits 4–31 zero.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
pub struct MemberDefinition {
    /// What the definition installs.
    pub kind: MemberKind,
    /// `[[Enumerable]]`: fields and object-literal members are, class
    /// methods and accessors are not.
    pub enumerable: bool,
    /// `[[Writable]]` is false. Only a [`MemberKind::Value`] can set it.
    pub read_only: bool,
}

impl MemberDefinition {
    const KIND_BITS: u32 = 0b11;
    const ENUMERABLE_BIT: u32 = 1 << 2;
    const READ_ONLY_BIT: u32 = 1 << 3;

    /// Encode with the documented bit layout.
    #[must_use]
    pub const fn to_imm32(self) -> i32 {
        let enumerable = if self.enumerable { Self::ENUMERABLE_BIT } else { 0 };
        let read_only = if self.read_only { Self::READ_ONLY_BIT } else { 0 };
        (self.kind as u32 | enumerable | read_only) as i32
    }

    /// Decode, rejecting reserved bits, an unknown kind and a read-only
    /// accessor.
    #[must_use]
    pub const fn from_imm32(value: i32) -> Option<Self> {
        let bits = value as u32;
        if bits & !(Self::KIND_BITS | Self::ENUMERABLE_BIT | Self::READ_ONLY_BIT) != 0 {
            return None;
        }
        let kind = match bits & Self::KIND_BITS {
            0 => MemberKind::Value,
            1 => MemberKind::Getter,
            2 => MemberKind::Setter,
            _ => return None,
        };
        let read_only = bits & Self::READ_ONLY_BIT != 0;
        if read_only && !matches!(kind, MemberKind::Value) {
            return None;
        }
        Some(Self {
            kind,
            enumerable: bits & Self::ENUMERABLE_BIT != 0,
            read_only,
        })
    }
}

/// Packed immediate of [`crate::Op::StoreRef`].
///
/// Bit layout: bits 0–15 slot ([`ContextCoord::RESERVED_SLOT`] = no slot,
/// the global form), bits 16–17 [`BindingStoreFallback`], bits 18–30 zero,
/// bit 31 strict.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
pub struct StoreRefMode {
    /// Slot written when the resolved base is a context; `None` when the
    /// reference was resolved against the global environment.
    pub slot: Option<u16>,
    /// Behavior of a store that lands on that context slot.
    pub fallback: BindingStoreFallback,
    /// Strictness of the extension and global store paths.
    pub strict: bool,
}

impl StoreRefMode {
    const SLOT_BITS: u32 = u16::MAX as u32;
    const FALLBACK_SHIFT: u32 = 16;
    const FALLBACK_BITS: u32 = 0b11 << Self::FALLBACK_SHIFT;
    const STRICT_BIT: u32 = 1 << 31;

    /// Encode with the documented bit layout.
    #[must_use]
    pub const fn to_imm32(self) -> i32 {
        let slot = match self.slot {
            Some(slot) => {
                debug_assert!(slot != ContextCoord::RESERVED_SLOT);
                slot as u32
            }
            None => ContextCoord::RESERVED_SLOT as u32,
        };
        let strict = if self.strict { Self::STRICT_BIT } else { 0 };
        (slot | ((self.fallback as u32) << Self::FALLBACK_SHIFT) | strict) as i32
    }

    /// Decode, rejecting reserved bits and an unknown fallback.
    #[must_use]
    pub const fn from_imm32(value: i32) -> Option<Self> {
        let bits = value as u32;
        if bits & !(Self::SLOT_BITS | Self::FALLBACK_BITS | Self::STRICT_BIT) != 0 {
            return None;
        }
        let Some(fallback) = BindingStoreFallback::from_imm32(
            ((bits & Self::FALLBACK_BITS) >> Self::FALLBACK_SHIFT) as i32,
        ) else {
            return None;
        };
        let slot = (bits & Self::SLOT_BITS) as u16;
        Some(Self {
            slot: if slot == ContextCoord::RESERVED_SLOT {
                None
            } else {
                Some(slot)
            },
            fallback,
            strict: bits & Self::STRICT_BIT != 0,
        })
    }
}

/// Typed read semantics and their authoritative operand roles.
///
/// Every field names an operand position of the instruction.
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
    /// TDZ-checked read of one context slot.
    ContextSlot {
        /// Result-register operand position.
        destination: u8,
        /// Context-register operand position.
        context: u8,
        /// [`ContextCoord`] immediate operand position.
        coord: u8,
    },
    /// Eval-extension probe at hops `[0, depth)`, then a TDZ-checked
    /// context-slot read.
    LookupSlot {
        /// Result-register operand position.
        destination: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// [`ContextCoord`] immediate operand position.
        coord: u8,
    },
    /// Eval-extension probe at hops `[0, depth)`, then the global
    /// Environment Record.
    LookupGlobal {
        /// Result-register operand position.
        destination: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// Hop-count immediate operand position.
        depth: u8,
        /// Unresolved-reference behavior.
        missing: BindingMissing,
    },
    /// Resolve an assignment target's reference base before its right-hand
    /// side runs (§13.15.2).
    ResolveRef {
        /// Reference-register operand position.
        destination: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// [`LookupRefTarget`] immediate operand position.
        target: u8,
    },
}

/// Typed write semantics and their authoritative operand roles.
///
/// Every field names an operand position of the instruction.
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
    /// TDZ-checked assignment of one context slot.
    ContextSlot {
        /// Boxed source-register operand position.
        value: u8,
        /// Context-register operand position.
        context: u8,
        /// [`ContextCoord`] immediate operand position.
        coord: u8,
    },
    /// BindThisValue into a derived-constructor `this` slot: a slot that is
    /// no longer the hole raises `ReferenceError`.
    BindThis {
        /// Boxed source-register operand position.
        value: u8,
        /// Context-register operand position.
        context: u8,
        /// [`ContextCoord`] immediate operand position.
        coord: u8,
    },
    /// Eval-extension probe at hops `[0, depth)`, then the context slot
    /// under a [`BindingStoreFallback`].
    LookupSlot {
        /// Boxed source-register operand position.
        value: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// [`ContextCoord`] immediate operand position.
        coord: u8,
        /// [`BindingStoreFallback`] immediate operand position.
        fallback: u8,
    },
    /// Eval-extension probe at hops `[0, depth)`, then the global
    /// `SetMutableBinding`.
    LookupGlobal {
        /// Boxed source-register operand position.
        value: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// [`LookupGlobalMode`] immediate operand position.
        mode: u8,
    },
    /// PutValue through a reference base resolved before the right-hand
    /// side.
    StoreRef {
        /// Boxed source-register operand position.
        value: u8,
        /// Reference-register operand position.
        reference: u8,
        /// String-constant operand position.
        name: u8,
        /// [`StoreRefMode`] immediate operand position.
        mode: u8,
    },
    /// Create-if-absent of an eval `var` in the var-scope extension.
    DeclareEvalVar {
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// Hop-count immediate operand position of the var-scope context.
        var_depth: u8,
    },
    /// Set-or-create in the var-scope extension.
    VarScope {
        /// Boxed source-register operand position.
        value: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// Hop-count immediate operand position of the var-scope context.
        var_depth: u8,
    },
}

/// Typed binding deletion semantics and authoritative operand roles.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum BindingDelete {
    /// Delete an eval-extension entry at hops `[0, depth)`; a miss leaves
    /// the declarative slot intact and yields `false`.
    LookupSlot {
        /// Boolean result-register operand position.
        destination: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// Hop-count immediate operand position.
        depth: u8,
    },
    /// Delete an eval-extension entry at hops `[0, depth)`, otherwise a
    /// global binding.
    LookupGlobal {
        /// Boolean result-register operand position.
        destination: u8,
        /// Context-register operand position.
        context: u8,
        /// String-constant operand position.
        name: u8,
        /// Hop-count immediate operand position.
        depth: u8,
    },
}

/// Single semantic authority for binding accesses that may throw or consult
/// eval extensions.
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
            | Self::Read(BindingRead::ContextSlot { destination, .. })
            | Self::Read(BindingRead::LookupSlot { destination, .. })
            | Self::Read(BindingRead::LookupGlobal { destination, .. })
            | Self::Read(BindingRead::ResolveRef { destination, .. })
            | Self::Delete(BindingDelete::LookupSlot { destination, .. })
            | Self::Delete(BindingDelete::LookupGlobal { destination, .. }) => Some(destination),
            Self::Write(_) => None,
        }
    }

    /// Every register operand the operation reads, as boxed inputs in ABI
    /// order: the stored value first, then the context or reference
    /// register (or the pre-RHS existence Boolean). A context register is a
    /// boxed input like any other value, so these operations take at most
    /// two register inputs.
    #[must_use]
    pub const fn value_operands(self) -> [Option<u8>; 2] {
        match self {
            Self::Read(BindingRead::GlobalThis { .. })
            | Self::Read(BindingRead::Global { .. })
            | Self::Read(BindingRead::Exists { .. }) => [None, None],
            Self::Read(BindingRead::ContextSlot { context, .. })
            | Self::Read(BindingRead::LookupSlot { context, .. })
            | Self::Read(BindingRead::LookupGlobal { context, .. })
            | Self::Read(BindingRead::ResolveRef { context, .. })
            | Self::Write(BindingWrite::DeclareEvalVar { context, .. })
            | Self::Delete(BindingDelete::LookupSlot { context, .. })
            | Self::Delete(BindingDelete::LookupGlobal { context, .. }) => [Some(context), None],
            Self::Write(BindingWrite::Global { value, .. }) => [Some(value), None],
            Self::Write(BindingWrite::GlobalChecked { value, exists, .. }) => {
                [Some(value), Some(exists)]
            }
            Self::Write(BindingWrite::ContextSlot { value, context, .. })
            | Self::Write(BindingWrite::BindThis { value, context, .. })
            | Self::Write(BindingWrite::LookupSlot { value, context, .. })
            | Self::Write(BindingWrite::LookupGlobal { value, context, .. })
            | Self::Write(BindingWrite::VarScope { value, context, .. }) => {
                [Some(value), Some(context)]
            }
            Self::Write(BindingWrite::StoreRef {
                value, reference, ..
            }) => [Some(value), Some(reference)],
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
    /// The opcode may read mutable managed-heap state — object, context,
    /// or eval-extension contents another instruction can change.
    /// Register, frame, constant-pool, and immutable closure-field reads
    /// are not heap reads.
    pub may_read_heap: bool,
    /// The opcode may store into a managed-heap object that existed before
    /// it ran. Initializing a freshly allocated object is not a heap write.
    pub may_write_heap: bool,
}

impl OpcodeEffects {
    /// A register or frame operation with no exception exit.
    const LEAF: Self = Self {
        may_throw: false,
        may_allocate: false,
        may_trigger_gc: false,
        may_reenter_javascript: false,
        safepoint_required: false,
        may_read_heap: false,
        may_write_heap: false,
    };

    /// Everything an opcode can do; the default for every row without an
    /// exact classification.
    const CONSERVATIVE: Self = Self {
        may_throw: true,
        may_allocate: true,
        may_trigger_gc: true,
        may_reenter_javascript: true,
        safepoint_required: true,
        may_read_heap: true,
        may_write_heap: true,
    };

    const fn throwing(self) -> Self {
        Self {
            may_throw: true,
            ..self
        }
    }

    const fn reading_heap(self) -> Self {
        Self {
            may_read_heap: true,
            ..self
        }
    }

    const fn writing_heap(self) -> Self {
        Self {
            may_write_heap: true,
            ..self
        }
    }

    /// Allocates (and may therefore collect or hit the heap limit, a
    /// catchable `RangeError`) without re-entering JavaScript.
    const fn allocating(self) -> Self {
        Self {
            may_throw: true,
            may_allocate: true,
            may_trigger_gc: true,
            safepoint_required: true,
            ..self
        }
    }
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
    (Op::LoadContextSlot, 0x32),
    (Op::StoreContextSlot, 0x33),
    (Op::Call, 0x34),
    (Op::CallWithThis, 0x35),
    (Op::BindFunction, 0x36),
    (Op::LoadThis, 0x37),
    (Op::LoadNewTarget, 0x38),
    (Op::Throw, 0x39),
    (Op::NewError, 0x3A),
    (Op::GetIterator, 0x3B),
    (Op::IteratorNext, 0x3C),
    (Op::ArrayPush, 0x3D),
    (Op::CallSpread, 0x3E),
    (Op::New, 0x3F),
    (Op::NewSpread, 0x40),
    (Op::SuperConstructSpread, 0x41),
    (Op::MakeClass, 0x42),
    (Op::MathLoad, 0x43),
    (Op::CollectRest, 0x44),
    (Op::ReturnValue, 0x45),
    (Op::ReturnUndefined, 0x46),
    (Op::NewObject, 0x47),
    (Op::LoadProperty, 0x48),
    (Op::StoreProperty, 0x49),
    (Op::DeleteProperty, 0x4A),
    (Op::GetPrototype, 0x4B),
    (Op::SetPrototype, 0x4C),
    (Op::NewArray, 0x4D),
    (Op::LoadElement, 0x4E),
    (Op::StoreElement, 0x4F),
    (Op::ArrayLength, 0x50),
    (Op::HasProperty, 0x51),
    (Op::Instanceof, 0x52),
    (Op::Eval, 0x53),
    (Op::NewFunction, 0x54),
    (Op::LoadGlobalThis, 0x55),
    (Op::LoadGlobalOrThrow, 0x56),
    (Op::CollectArguments, 0x57),
    (Op::LoadGlobalOrUndefined, 0x58),
    (Op::DefineGlobalVar, 0x59),
    (Op::ImportMetaResolve, 0x5A),
    (Op::ImportNamespaceDynamic, 0x5B),
    (Op::ImportNamespace, 0x5C),
    (Op::PromiseFulfilledOf, 0x5D),
    (Op::TemporalLoad, 0x5E),
    (Op::NewCollection, 0x5F),
    (Op::NewWeakRef, 0x60),
    (Op::NewFinalizationRegistry, 0x61),
    (Op::SymbolLoad, 0x62),
    (Op::TypeOf, 0x63),
    (Op::DeleteElement, 0x64),
    (Op::Await, 0x65),
    (Op::SameValue, 0x66),
    (Op::IsArray, 0x67),
    (Op::LooseEqual, 0x68),
    (Op::LooseNotEqual, 0x69),
    (Op::NewBuiltinError, 0x6A),
    (Op::LoadBuiltinError, 0x6B),
    (Op::BigIntCall, 0x6C),
    (Op::ArrayConstruct, 0x6D),
    (Op::ArrayFrom, 0x6E),
    (Op::ArrayOf, 0x6F),
    (Op::ArrayBufferCall, 0x70),
    (Op::DataViewCall, 0x71),
    (Op::Yield, 0x72),
    (Op::SharedArrayBufferCall, 0x73),
    (Op::ToPrimitive, 0x74),
    (Op::ForInKeys, 0x75),
    (Op::CopyDataProperties, 0x76),
    (Op::DefineOwnProperty, 0x77),
    (Op::IteratorClose, 0x78),
    (Op::IteratorCloseThrow, 0x79),
    (Op::GeneratorStart, 0x7A),
    (Op::GetAsyncIterator, 0x7B),
    (Op::BindThisValue, 0x7C),
    (Op::LoadSuperProperty, 0x7D),
    (Op::LoadSuperElement, 0x7E),
    (Op::SetSuperProperty, 0x7F),
    (Op::SetSuperElement, 0x80),
    (Op::CreateContext, 0x81),
    (Op::ImportNamespaceDeferred, 0x82),
    (Op::EvaluateModule, 0x83),
    (Op::MarkModuleEvaluated, 0x84),
    (Op::StarReexport, 0x85),
    (Op::ModuleNamespaceObject, 0x86),
    (Op::LoadImportBinding, 0x87),
    (Op::StoreContextSlotChecked, 0x88),
    (Op::DeclareGlobalVar, 0x89),
    (Op::LoadLookupGlobal, 0x8A),
    (Op::StoreLookupGlobal, 0x8B),
    (Op::TypeofLookupGlobal, 0x8C),
    (Op::DefineGlobalFunction, 0x8D),
    (Op::DeclareGlobalLex, 0x8E),
    (Op::StoreGlobalBinding, 0x8F),
    (Op::InitGlobalLex, 0x90),
    (Op::ValidateGlobalDecl, 0x91),
    (Op::ToObject, 0x92),
    (Op::ToNumeric, 0x93),
    (Op::PrivateGet, 0x94),
    (Op::PrivateSet, 0x95),
    (Op::YieldDelegate, 0x96),
    (Op::DefineDataProperty, 0x97),
    (Op::SetFunctionName, 0x98),
    (Op::ClassCheck, 0x99),
    (Op::ToPropertyKey, 0x9A),
    (Op::Increment, 0x9B),
    (Op::PrivateBrandCheck, 0x9C),
    (Op::LoadLookupSlot, 0x9D),
    (Op::GetTemplateObject, 0x9E),
    (Op::DeleteLookupGlobal, 0x9F),
    (Op::NewPrivateName, 0xA0),
    (Op::TailCall, 0xA1),
    (Op::IsEvalIntrinsic, 0xA2),
    (Op::GlobalBindingExists, 0xA3),
    (Op::StoreGlobalChecked, 0xA4),
    (Op::AddImm, 0xA5),
    (Op::SubImm, 0xA6),
    (Op::BitwiseAndImm, 0xA7),
    (Op::LessThanImm, 0xA8),
    (Op::EqualImm, 0xA9),
    (Op::NotEqualImm, 0xAA),
    (Op::SuperConstruct, 0xAB),
    (Op::StoreLookupSlot, 0xAC),
    (Op::DeleteLookupSlot, 0xAD),
    (Op::AsyncIteratorReturn, 0xAE),
    (Op::CheckIteratorResult, 0xAF),
    (Op::ResolveLookupRef, 0xB0),
    (Op::LoadContextSlotChecked, 0xB1),
    (Op::StoreRef, 0xB2),
    (Op::DeclareEvalVar, 0xB3),
    (Op::StorePropertyStrict, 0xB4),
    (Op::StoreElementStrict, 0xB5),
    (Op::CallForwardArguments, 0xB6),
    (Op::NewObjectLiteral, 0xB7),
    (Op::LoadArgumentsLength, 0xB8),
    (Op::LoadArgumentsElement, 0xB9),
    (Op::LoadClosureContext, 0xBA),
    (Op::LoadSelf, 0xBB),
    (Op::CopyContext, 0xBC),
    (Op::BindThisContextSlot, 0xBD),
    (Op::ReturnDerived, 0xBE),
    (Op::StoreVarScope, 0xBF),
    (Op::TestTypeOf, 0xC0),
    (Op::SpreadAppend, 0xC1),
    (Op::DefineMember, 0xC2),
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
const CALL_FORWARD_ARGUMENTS: &[OperandSpec] = &[W, R, R, R, R];
const COUNTED_VALUES_PREFIX: &[OperandSpec] = &[W, CONST];
const OBJECT_LITERAL_PREFIX: &[OperandSpec] = &[W, CONST, CONST];
const METHOD_CALL_PREFIX: &[OperandSpec] = &[W, R, CONST, CONST];
const NAMESPACE_CALL_PREFIX: &[OperandSpec] = &[W, CONST, CONST];
const WRITE_READ_CONST: &[OperandSpec] = &[W, R, CONST];
const WRITE_CONST_CONST: &[OperandSpec] = &[W, CONST, CONST];
const READ_CONST_READ_WRITE: &[OperandSpec] = &[R, CONST, R, W];
const READ_READ: &[OperandSpec] = &[R, R];
const READ_READ_READ: &[OperandSpec] = &[R, R, R];
const WRITE_WRITE_READ: &[OperandSpec] = &[W, W, R];
const READ_CONST: &[OperandSpec] = &[R, CONST];
const CONST_READ: &[OperandSpec] = &[CONST, R];
const WRITE_READ_WRITE: &[OperandSpec] = &[W, R, W];
const WRITE_READ_READ_READ: &[OperandSpec] = &[W, R, R, R];
const WRITE_FOUR_READS: &[OperandSpec] = &[W, R, R, R, R];
const WRITE_READ_IMM: &[OperandSpec] = &[W, R, IMM];
const CONST_READ_IMM: &[OperandSpec] = &[CONST, R, IMM];
const CONST_IMM: &[OperandSpec] = &[CONST, IMM];
const READ_CONST_IMM: &[OperandSpec] = &[R, CONST, IMM];
const SCOPE: OperandSpec = OperandSpec::immediate(ImmediateDomain::ScopeIndex);
const COORD: OperandSpec = OperandSpec::immediate(ImmediateDomain::ContextCoord);
const DEPTH: OperandSpec = OperandSpec::immediate(ImmediateDomain::ContextDepth);
const REF_TARGET: OperandSpec = OperandSpec::immediate(ImmediateDomain::LookupRefTarget);
const GLOBAL_MODE: OperandSpec = OperandSpec::immediate(ImmediateDomain::LookupGlobalMode);
const FALLBACK: OperandSpec = OperandSpec::immediate(ImmediateDomain::StoreFallback);
const REF_MODE: OperandSpec = OperandSpec::immediate(ImmediateDomain::StoreRefMode);
const MEMBER_DEFINITION: OperandSpec = OperandSpec::immediate(ImmediateDomain::MemberDefinition);
const CREATE_CONTEXT: &[OperandSpec] = &[W, R, SCOPE];
const LOAD_CONTEXT_SLOT: &[OperandSpec] = &[W, R, COORD];
const STORE_CONTEXT_SLOT: &[OperandSpec] = &[R, R, COORD];
const MAKE_CLOSURE: &[OperandSpec] = &[W, CONST, R];
const LOAD_LOOKUP_SLOT: &[OperandSpec] = &[W, R, CONST, COORD];
const STORE_LOOKUP_SLOT: &[OperandSpec] = &[R, R, CONST, COORD, FALLBACK];
const LOOKUP_BY_DEPTH: &[OperandSpec] = &[W, R, CONST, DEPTH];
const STORE_LOOKUP_GLOBAL: &[OperandSpec] = &[R, R, CONST, GLOBAL_MODE];
const RESOLVE_LOOKUP_REF: &[OperandSpec] = &[W, R, CONST, REF_TARGET];
const STORE_REF: &[OperandSpec] = &[R, R, CONST, REF_MODE];
const DECLARE_EVAL_VAR: &[OperandSpec] = &[R, CONST, DEPTH];
const STORE_VAR_SCOPE: &[OperandSpec] = &[R, R, CONST, DEPTH];
const EVAL: &[OperandSpec] = &[W, R, R, IMM];

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
        | Op::LoadGlobalThis
        | Op::LoadClosureContext
        | Op::LoadSelf => OperandShape::Fixed(WRITE),
        Op::LoadString | Op::LoadNumber | Op::LoadBigInt | Op::LoadRegExp => {
            OperandShape::Fixed(WRITE_CONST)
        }
        Op::LoadInt32 => OperandShape::Fixed(WRITE_IMM),
        Op::Jump => OperandShape::Fixed(JUMP),
        Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => OperandShape::Fixed(BRANCH),
        Op::Return | Op::ReturnValue => OperandShape::Fixed(&[R]),
        Op::ReturnUndefined => OperandShape::Fixed(EMPTY),
        Op::ReturnDerived => OperandShape::Fixed(STORE_CONTEXT_SLOT),
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
        Op::NewObjectLiteral => OperandShape::Variadic {
            prefix: OBJECT_LITERAL_PREFIX,
            count_operand_index: 1,
            tail: R,
        },
        Op::CallMethodValue => OperandShape::Variadic {
            prefix: METHOD_CALL_PREFIX,
            count_operand_index: 3,
            tail: R,
        },
        Op::CallForwardArguments => OperandShape::Fixed(CALL_FORWARD_ARGUMENTS),
        Op::Throw => OperandShape::Fixed(&[R]),
        Op::NewObject => OperandShape::Fixed(WRITE),
        Op::LoadProperty | Op::DeleteProperty => OperandShape::Fixed(WRITE_READ_CONST),
        Op::StoreProperty => OperandShape::Fixed(READ_CONST_READ_WRITE),
        Op::StorePropertyStrict => OperandShape::Fixed(READ_CONST_READ_WRITE),
        Op::GetPrototype | Op::ArrayLength | Op::GetIterator | Op::GetAsyncIterator => {
            OperandShape::Fixed(WRITE_READ)
        }
        Op::SetPrototype | Op::ArrayPush | Op::SpreadAppend => OperandShape::Fixed(READ_READ),
        Op::CopyDataProperties => OperandShape::Fixed(READ_READ_READ),
        Op::LoadElement | Op::DeleteElement | Op::HasProperty | Op::Instanceof => {
            OperandShape::Fixed(WRITE_READ_READ)
        }
        Op::StoreElement => OperandShape::Fixed(READ_READ_READ),
        Op::StoreElementStrict => OperandShape::Fixed(READ_READ_READ),
        Op::IteratorNext => OperandShape::Fixed(WRITE_WRITE_READ),
        Op::IteratorClose | Op::IteratorCloseThrow | Op::CheckIteratorResult => {
            OperandShape::Fixed(&[R])
        }
        Op::AsyncIteratorReturn => OperandShape::Fixed(WRITE_WRITE_READ),
        Op::ForInKeys => OperandShape::Fixed(WRITE_READ),
        Op::CreateContext => OperandShape::Fixed(CREATE_CONTEXT),
        Op::CopyContext => OperandShape::Fixed(WRITE_READ),
        Op::LoadContextSlot | Op::LoadContextSlotChecked => OperandShape::Fixed(LOAD_CONTEXT_SLOT),
        Op::StoreContextSlot | Op::StoreContextSlotChecked | Op::BindThisContextSlot => {
            OperandShape::Fixed(STORE_CONTEXT_SLOT)
        }
        Op::LoadLookupSlot => OperandShape::Fixed(LOAD_LOOKUP_SLOT),
        Op::StoreLookupSlot => OperandShape::Fixed(STORE_LOOKUP_SLOT),
        Op::DeleteLookupSlot
        | Op::LoadLookupGlobal
        | Op::TypeofLookupGlobal
        | Op::DeleteLookupGlobal => OperandShape::Fixed(LOOKUP_BY_DEPTH),
        Op::StoreLookupGlobal => OperandShape::Fixed(STORE_LOOKUP_GLOBAL),
        Op::ResolveLookupRef => OperandShape::Fixed(RESOLVE_LOOKUP_REF),
        Op::StoreRef => OperandShape::Fixed(STORE_REF),
        Op::DeclareEvalVar => OperandShape::Fixed(DECLARE_EVAL_VAR),
        Op::StoreVarScope => OperandShape::Fixed(STORE_VAR_SCOPE),
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
        Op::MakeClosure => OperandShape::Fixed(MAKE_CLOSURE),
        Op::GeneratorStart => OperandShape::Fixed(EMPTY),
        Op::NewError
        | Op::ImportMetaResolve
        | Op::PromiseFulfilledOf
        | Op::NewWeakRef
        | Op::NewFinalizationRegistry => OperandShape::Fixed(WRITE_READ),
        Op::CallSpread => OperandShape::Fixed(WRITE_READ_READ_READ),
        Op::NewSpread | Op::SuperConstructSpread | Op::ImportNamespaceDynamic => {
            OperandShape::Fixed(WRITE_READ_READ)
        }
        Op::MakeClass => OperandShape::Fixed(WRITE_FOUR_READS),
        Op::CollectRest | Op::LoadArgumentsLength => OperandShape::Fixed(WRITE),
        Op::CollectArguments => OperandShape::Fixed(WRITE_READ),
        Op::LoadArgumentsElement => OperandShape::Fixed(WRITE_READ),
        Op::Increment | Op::TestTypeOf => OperandShape::Fixed(WRITE_READ_IMM),
        Op::Eval => OperandShape::Fixed(EVAL),
        Op::DefineGlobalVar => OperandShape::Fixed(CONST_READ),
        Op::NewCollection | Op::NewBuiltinError => OperandShape::Fixed(&[W, CONST, R]),
        Op::ToPrimitive => OperandShape::Fixed(WRITE_READ_CONST),
        Op::DefineOwnProperty | Op::PrivateSet | Op::DefineDataProperty => {
            OperandShape::Fixed(&[R, R, R])
        }
        Op::DefineMember => OperandShape::Fixed(&[R, R, R, MEMBER_DEFINITION]),
        Op::Yield | Op::YieldDelegate => OperandShape::Fixed(WRITE_WRITE_READ),
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
        Op::Return | Op::ReturnValue | Op::ReturnUndefined | Op::ReturnDerived => {
            SuccessorShape::new(RETURN_SUCCESSORS)
        }
        Op::TailCall => SuccessorShape::new(TAIL_CALL_SUCCESSORS),
        Op::Throw => SuccessorShape::new(NO_NORMAL_SUCCESSORS),
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
            | ControlFlow::Suspend => unreachable!(),
        },
    }
}

const THROW_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] =
    &[ExceptionSuccessorSpec::HandlerTableOrCaller];
const RETURN_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] =
    &[ExceptionSuccessorSpec::CallerHandlerOrUncaught];
const NO_EXCEPTION_SUCCESSORS: &[ExceptionSuccessorSpec] = &[];

const fn exception_successor_shape(op: Op) -> ExceptionSuccessorShape {
    match op {
        Op::Throw => ExceptionSuccessorShape::new(THROW_EXCEPTION_SUCCESSORS),
        Op::Return | Op::ReturnValue | Op::ReturnUndefined | Op::ReturnDerived => {
            ExceptionSuccessorShape::new(RETURN_EXCEPTION_SUCCESSORS)
        }
        _ if !effects(op).may_throw => ExceptionSuccessorShape::new(NO_EXCEPTION_SUCCESSORS),
        _ => ExceptionSuccessorShape::new(THROW_EXCEPTION_SUCCESSORS),
    }
}

const fn control_flow(op: Op) -> ControlFlow {
    match op {
        Op::Jump => ControlFlow::Jump,
        Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => ControlFlow::Branch,
        Op::Return | Op::ReturnValue | Op::ReturnUndefined | Op::ReturnDerived | Op::TailCall => {
            ControlFlow::Return
        }
        Op::Throw => ControlFlow::Throw,
        Op::Await | Op::Yield | Op::YieldDelegate | Op::GeneratorStart => ControlFlow::Suspend,
        Op::Call
        | Op::CallWithThis
        | Op::CallForwardArguments
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
        Op::LoadContextSlotChecked => Some(BindingSemantics::Read(BindingRead::ContextSlot {
            destination: 0,
            context: 1,
            coord: 2,
        })),
        Op::LoadLookupSlot => Some(BindingSemantics::Read(BindingRead::LookupSlot {
            destination: 0,
            context: 1,
            name: 2,
            coord: 3,
        })),
        Op::LoadLookupGlobal => Some(BindingSemantics::Read(BindingRead::LookupGlobal {
            destination: 0,
            context: 1,
            name: 2,
            depth: 3,
            missing: BindingMissing::Throw,
        })),
        Op::TypeofLookupGlobal => Some(BindingSemantics::Read(BindingRead::LookupGlobal {
            destination: 0,
            context: 1,
            name: 2,
            depth: 3,
            missing: BindingMissing::Undefined,
        })),
        Op::ResolveLookupRef => Some(BindingSemantics::Read(BindingRead::ResolveRef {
            destination: 0,
            context: 1,
            name: 2,
            target: 3,
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
        Op::StoreContextSlotChecked => Some(BindingSemantics::Write(BindingWrite::ContextSlot {
            value: 0,
            context: 1,
            coord: 2,
        })),
        Op::BindThisContextSlot => Some(BindingSemantics::Write(BindingWrite::BindThis {
            value: 0,
            context: 1,
            coord: 2,
        })),
        Op::StoreLookupSlot => Some(BindingSemantics::Write(BindingWrite::LookupSlot {
            value: 0,
            context: 1,
            name: 2,
            coord: 3,
            fallback: 4,
        })),
        Op::StoreLookupGlobal => Some(BindingSemantics::Write(BindingWrite::LookupGlobal {
            value: 0,
            context: 1,
            name: 2,
            mode: 3,
        })),
        Op::StoreRef => Some(BindingSemantics::Write(BindingWrite::StoreRef {
            value: 0,
            reference: 1,
            name: 2,
            mode: 3,
        })),
        Op::DeclareEvalVar => Some(BindingSemantics::Write(BindingWrite::DeclareEvalVar {
            context: 0,
            name: 1,
            var_depth: 2,
        })),
        Op::StoreVarScope => Some(BindingSemantics::Write(BindingWrite::VarScope {
            value: 0,
            context: 1,
            name: 2,
            var_depth: 3,
        })),
        Op::DeleteLookupSlot => Some(BindingSemantics::Delete(BindingDelete::LookupSlot {
            destination: 0,
            context: 1,
            name: 2,
            depth: 3,
        })),
        Op::DeleteLookupGlobal => Some(BindingSemantics::Delete(BindingDelete::LookupGlobal {
            destination: 0,
            context: 1,
            name: 2,
            depth: 3,
        })),
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
        | Op::CallForwardArguments
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
    match op {
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
        | Op::LoadNewTarget
        | Op::LoadClosureContext
        | Op::LoadSelf
        | Op::Jump
        | Op::JumpIfTrue
        | Op::JumpIfFalse
        | Op::JumpIfNullish => OpcodeEffects::LEAF,
        // Allocation-free, non-reentrant leaves that own a typed exception
        // exit: `LoadThis` can hit the derived-`this` TDZ, and a return
        // completion can fail only after leaving this frame.
        Op::LoadThis | Op::Return | Op::ReturnValue | Op::ReturnUndefined | Op::ReturnDerived => {
            OpcodeEffects::LEAF.throwing()
        }
        // Plain context memory: a load, and a write-barriered store. The
        // barrier records an edge and never collects.
        Op::LoadContextSlot => OpcodeEffects::LEAF.reading_heap(),
        Op::StoreContextSlot => OpcodeEffects::LEAF.writing_heap(),
        // Hole-checked context accesses raise their `ReferenceError` from
        // the exception exit without allocating on the success path.
        Op::LoadContextSlotChecked => OpcodeEffects::LEAF.reading_heap().throwing(),
        Op::StoreContextSlotChecked | Op::BindThisContextSlot => {
            OpcodeEffects::LEAF.reading_heap().writing_heap().throwing()
        }
        // Non-reentrant allocations of a context or a closure.
        Op::CreateContext | Op::CopyContext | Op::MakeClosure => {
            OpcodeEffects::LEAF.reading_heap().allocating()
        }
        _ => OpcodeEffects::CONSERVATIVE,
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
        | Op::LoadArgumentsLength
        | Op::LoadArgumentsElement
        | Op::Call
        | Op::CallWithThis
        | Op::CallForwardArguments
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
            Op::LoadContextSlotChecked,
            Op::StoreContextSlotChecked,
            Op::BindThisContextSlot,
            Op::LoadLookupSlot,
            Op::StoreLookupSlot,
            Op::DeleteLookupSlot,
            Op::LoadLookupGlobal,
            Op::TypeofLookupGlobal,
            Op::StoreLookupGlobal,
            Op::DeleteLookupGlobal,
            Op::ResolveLookupRef,
            Op::StoreRef,
            Op::DeclareEvalVar,
            Op::StoreVarScope,
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
    fn context_coordinates_pack_depth_over_slot() {
        for (depth, slot) in [(0, 0), (1, 2), (7, 300), (u16::MAX, ContextCoord::MAX_SLOT)] {
            let coord = ContextCoord::new(depth, slot).expect("addressable slot");
            let imm = coord.to_imm32();
            assert_eq!(imm as u32, (u32::from(depth) << 16) | u32::from(slot));
            assert_eq!(ContextCoord::from_imm32(imm), Some(coord));
            assert_eq!(
                LookupRefTarget::from_imm32(imm),
                LookupRefTarget::Slot(coord)
            );
        }
        assert_eq!(ContextCoord::new(3, ContextCoord::RESERVED_SLOT), None);
        assert_eq!(ContextCoord::from_imm32(0x0003_FFFF), None);
        assert_eq!(ContextCoord::from_imm32(-1), None);

        for depth in [0, 3, u16::MAX] {
            let target = LookupRefTarget::Global { depth };
            assert_eq!(LookupRefTarget::from_imm32(target.to_imm32()), target);
            assert_eq!(target.depth(), depth);
            assert!(ImmediateDomain::LookupRefTarget.admits(target.to_imm32()));
            assert!(!ImmediateDomain::ContextCoord.admits(target.to_imm32()));
        }
    }

    #[test]
    fn lookup_global_mode_packs_depth_and_strict_bit() {
        for depth in [0, 1, 0x1234, u16::MAX] {
            for strict in [false, true] {
                let mode = LookupGlobalMode { depth, strict };
                let imm = mode.to_imm32();
                assert_eq!(
                    imm as u32,
                    u32::from(depth) | if strict { 1 << 31 } else { 0 }
                );
                assert_eq!(LookupGlobalMode::from_imm32(imm), Some(mode));
            }
        }
        for reserved in [1 << 16, 1 << 30, 0x7FFF_0000] {
            assert_eq!(LookupGlobalMode::from_imm32(reserved), None);
            assert!(!ImmediateDomain::LookupGlobalMode.admits(reserved));
        }
    }

    #[test]
    fn store_ref_mode_owns_its_bit_layout() {
        for slot in [Some(0), Some(9), Some(ContextCoord::MAX_SLOT), None] {
            for fallback in [
                BindingStoreFallback::Mutable,
                BindingStoreFallback::ImmutableThrow,
                BindingStoreFallback::ImmutableIgnore,
            ] {
                for strict in [false, true] {
                    let mode = StoreRefMode {
                        slot,
                        fallback,
                        strict,
                    };
                    let imm = mode.to_imm32() as u32;
                    assert_eq!(
                        imm & 0xFFFF,
                        u32::from(slot.unwrap_or(ContextCoord::RESERVED_SLOT))
                    );
                    assert_eq!((imm >> 16) & 0b11, fallback.to_imm32() as u32);
                    assert_eq!(imm >> 31, u32::from(strict));
                    assert_eq!(imm & 0x7FFC_0000, 0);
                    assert_eq!(StoreRefMode::from_imm32(imm as i32), Some(mode));
                }
            }
        }
        // Fallback 3 is outside the alphabet; bits 18–30 are reserved.
        assert_eq!(StoreRefMode::from_imm32(3 << 16), None);
        assert_eq!(StoreRefMode::from_imm32(1 << 18), None);
        assert_eq!(StoreRefMode::from_imm32(1 << 30), None);
        for value in [-1, 3, 4, i32::MIN] {
            assert_eq!(BindingStoreFallback::from_imm32(value), None);
        }
    }

    fn assert_row(
        op: Op,
        byte: u8,
        operands: &[(OperandKind, RegisterAccess, Option<ImmediateDomain>)],
        binding: Option<BindingSemantics>,
    ) {
        let schema = opcode_schema(op);
        assert_eq!(schema.byte, byte, "{op:?} byte");
        let shape = schema.operand_shape.fixed().expect("fixed shape");
        assert_eq!(shape.len(), operands.len(), "{op:?} arity");
        for (index, (spec, (kind, access, domain))) in shape.iter().zip(operands).enumerate() {
            assert_eq!(spec.kind, *kind, "{op:?} operand {index} kind");
            assert_eq!(
                spec.register_access, *access,
                "{op:?} operand {index} access"
            );
            assert_eq!(spec.imm_domain, *domain, "{op:?} operand {index} domain");
        }
        assert_eq!(schema.binding, binding, "{op:?} binding");
        assert_eq!(
            schema.control_flow == ControlFlow::Return,
            op == Op::ReturnDerived
        );
    }

    #[test]
    fn context_family_rows_are_exact() {
        use OperandKind::{ConstIndex as K, Imm32 as I, Register as Reg};
        use RegisterAccess::{None as N, Read as Rd, Write as Wr};
        let w = (Reg, Wr, None);
        let r = (Reg, Rd, None);
        let k = (K, N, None);
        let coord = (I, N, Some(ImmediateDomain::ContextCoord));

        assert_row(Op::LoadClosureContext, 0xBA, &[w], None);
        assert_row(Op::LoadSelf, 0xBB, &[w], None);
        assert_row(
            Op::CreateContext,
            0x81,
            &[w, r, (I, N, Some(ImmediateDomain::ScopeIndex))],
            None,
        );
        assert_row(Op::CopyContext, 0xBC, &[w, r], None);
        assert_row(Op::LoadContextSlot, 0x32, &[w, r, coord], None);
        assert_row(Op::StoreContextSlot, 0x33, &[r, r, coord], None);
        assert_row(
            Op::LoadContextSlotChecked,
            0xB1,
            &[w, r, coord],
            Some(BindingSemantics::Read(BindingRead::ContextSlot {
                destination: 0,
                context: 1,
                coord: 2,
            })),
        );
        assert_row(
            Op::StoreContextSlotChecked,
            0x88,
            &[r, r, coord],
            Some(BindingSemantics::Write(BindingWrite::ContextSlot {
                value: 0,
                context: 1,
                coord: 2,
            })),
        );
        assert_row(
            Op::BindThisContextSlot,
            0xBD,
            &[r, r, coord],
            Some(BindingSemantics::Write(BindingWrite::BindThis {
                value: 0,
                context: 1,
                coord: 2,
            })),
        );
        assert_row(Op::ReturnDerived, 0xBE, &[r, r, coord], None);
        assert_row(Op::MakeClosure, 0x31, &[w, k, r], None);
        assert_row(Op::Eval, 0x53, &[w, r, r, (I, N, None)], None);
        assert_row(Op::CallForwardArguments, 0xB6, &[w, r, r, r, r], None);
    }

    #[test]
    fn lookup_family_rows_are_exact() {
        use OperandKind::{ConstIndex as K, Imm32 as I, Register as Reg};
        use RegisterAccess::{None as N, Read as Rd, Write as Wr};
        let w = (Reg, Wr, None);
        let r = (Reg, Rd, None);
        let k = (K, N, None);
        let coord = (I, N, Some(ImmediateDomain::ContextCoord));
        let depth = (I, N, Some(ImmediateDomain::ContextDepth));

        assert_row(
            Op::LoadLookupSlot,
            0x9D,
            &[w, r, k, coord],
            Some(BindingSemantics::Read(BindingRead::LookupSlot {
                destination: 0,
                context: 1,
                name: 2,
                coord: 3,
            })),
        );
        assert_row(
            Op::StoreLookupSlot,
            0xAC,
            &[r, r, k, coord, (I, N, Some(ImmediateDomain::StoreFallback))],
            Some(BindingSemantics::Write(BindingWrite::LookupSlot {
                value: 0,
                context: 1,
                name: 2,
                coord: 3,
                fallback: 4,
            })),
        );
        assert_row(
            Op::DeleteLookupSlot,
            0xAD,
            &[w, r, k, depth],
            Some(BindingSemantics::Delete(BindingDelete::LookupSlot {
                destination: 0,
                context: 1,
                name: 2,
                depth: 3,
            })),
        );
        for (op, byte, missing) in [
            (Op::LoadLookupGlobal, 0x8A, BindingMissing::Throw),
            (Op::TypeofLookupGlobal, 0x8C, BindingMissing::Undefined),
        ] {
            assert_row(
                op,
                byte,
                &[w, r, k, depth],
                Some(BindingSemantics::Read(BindingRead::LookupGlobal {
                    destination: 0,
                    context: 1,
                    name: 2,
                    depth: 3,
                    missing,
                })),
            );
        }
        assert_row(
            Op::StoreLookupGlobal,
            0x8B,
            &[r, r, k, (I, N, Some(ImmediateDomain::LookupGlobalMode))],
            Some(BindingSemantics::Write(BindingWrite::LookupGlobal {
                value: 0,
                context: 1,
                name: 2,
                mode: 3,
            })),
        );
        assert_row(
            Op::DeleteLookupGlobal,
            0x9F,
            &[w, r, k, depth],
            Some(BindingSemantics::Delete(BindingDelete::LookupGlobal {
                destination: 0,
                context: 1,
                name: 2,
                depth: 3,
            })),
        );
        assert_row(
            Op::ResolveLookupRef,
            0xB0,
            &[w, r, k, (I, N, Some(ImmediateDomain::LookupRefTarget))],
            Some(BindingSemantics::Read(BindingRead::ResolveRef {
                destination: 0,
                context: 1,
                name: 2,
                target: 3,
            })),
        );
        assert_row(
            Op::StoreRef,
            0xB2,
            &[r, r, k, (I, N, Some(ImmediateDomain::StoreRefMode))],
            Some(BindingSemantics::Write(BindingWrite::StoreRef {
                value: 0,
                reference: 1,
                name: 2,
                mode: 3,
            })),
        );
        assert_row(
            Op::DeclareEvalVar,
            0xB3,
            &[r, k, depth],
            Some(BindingSemantics::Write(BindingWrite::DeclareEvalVar {
                context: 0,
                name: 1,
                var_depth: 2,
            })),
        );
        assert_row(
            Op::StoreVarScope,
            0xBF,
            &[r, r, k, depth],
            Some(BindingSemantics::Write(BindingWrite::VarScope {
                value: 0,
                context: 1,
                name: 2,
                var_depth: 3,
            })),
        );
    }

    #[test]
    fn context_family_effects_are_exact() {
        let effects = |op| opcode_schema(op).effects;
        for op in [Op::LoadClosureContext, Op::LoadSelf] {
            assert_eq!(effects(op), OpcodeEffects::LEAF, "{op:?}");
        }
        let load = effects(Op::LoadContextSlot);
        assert!(load.may_read_heap && !load.may_write_heap && !load.may_throw);
        let store = effects(Op::StoreContextSlot);
        assert!(store.may_write_heap && !store.may_throw && !store.may_allocate);
        assert!(!store.safepoint_required);
        for op in [
            Op::LoadContextSlotChecked,
            Op::StoreContextSlotChecked,
            Op::BindThisContextSlot,
        ] {
            let checked = effects(op);
            assert!(checked.may_throw && !checked.may_allocate, "{op:?}");
            assert!(
                !checked.may_trigger_gc && !checked.safepoint_required,
                "{op:?}"
            );
            assert_eq!(
                checked.may_write_heap,
                op != Op::LoadContextSlotChecked,
                "{op:?}"
            );
        }
        for op in [Op::CreateContext, Op::CopyContext, Op::MakeClosure] {
            let allocation = effects(op);
            assert!(
                allocation.may_allocate && allocation.may_trigger_gc,
                "{op:?}"
            );
            assert!(
                allocation.safepoint_required && allocation.may_throw,
                "{op:?}"
            );
            assert!(!allocation.may_reenter_javascript, "{op:?}");
            assert!(!allocation.may_write_heap, "{op:?}");
        }
        // Unchecked context accesses have no exception edge; checked ones do.
        for op in [Op::LoadContextSlot, Op::StoreContextSlot] {
            assert!(
                opcode_schema(op)
                    .exception_successor_shape
                    .exact()
                    .is_empty()
            );
        }
        for op in [Op::LoadContextSlotChecked, Op::StoreContextSlotChecked] {
            assert_eq!(
                opcode_schema(op).exception_successor_shape.exact(),
                &[ExceptionSuccessorSpec::HandlerTableOrCaller]
            );
        }
        let return_derived = opcode_schema(Op::ReturnDerived);
        assert_eq!(return_derived.effects, OpcodeEffects::LEAF.throwing());
        assert_eq!(
            return_derived.successor_shape.exact(),
            &[SuccessorSpec::FrameReturn]
        );
        assert_eq!(
            return_derived.exception_successor_shape.exact(),
            &[ExceptionSuccessorSpec::CallerHandlerOrUncaught]
        );
        assert!(Op::ReturnDerived.is_branch());
    }

    #[test]
    fn binding_semantic_operands_agree_with_wire_roles() {
        let assert_operand =
            |schema: &OpcodeSchema, index: u8, kind: OperandKind, access: RegisterAccess| {
                let actual = operand_spec_at(schema.op, usize::from(index))
                    .unwrap_or_else(|| panic!("{:?} operand {index} is missing", schema.op));
                assert_eq!(
                    (actual.kind, actual.register_access),
                    (kind, access),
                    "{:?} operand {index}",
                    schema.op
                );
            };
        let assert_immediate = |schema: &OpcodeSchema, index: u8, domain: ImmediateDomain| {
            assert_operand(schema, index, OperandKind::Imm32, RegisterAccess::None);
            assert_eq!(
                operand_spec_at(schema.op, usize::from(index)).and_then(|spec| spec.imm_domain),
                Some(domain),
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
            let name = |index| {
                assert_operand(schema, index, OperandKind::ConstIndex, RegisterAccess::None);
            };
            match binding {
                BindingSemantics::Read(BindingRead::GlobalThis { .. }) => {}
                BindingSemantics::Read(BindingRead::Global { name: n, .. })
                | BindingSemantics::Read(BindingRead::Exists { name: n, .. })
                | BindingSemantics::Write(BindingWrite::GlobalChecked { name: n, .. }) => name(n),
                BindingSemantics::Write(BindingWrite::Global {
                    name: n, strict, ..
                }) => {
                    name(n);
                    assert_operand(schema, strict, OperandKind::Imm32, RegisterAccess::None);
                }
                BindingSemantics::Read(BindingRead::ContextSlot { coord, .. })
                | BindingSemantics::Write(BindingWrite::ContextSlot { coord, .. })
                | BindingSemantics::Write(BindingWrite::BindThis { coord, .. }) => {
                    assert_immediate(schema, coord, ImmediateDomain::ContextCoord);
                }
                BindingSemantics::Read(BindingRead::LookupSlot { name: n, coord, .. }) => {
                    name(n);
                    assert_immediate(schema, coord, ImmediateDomain::ContextCoord);
                }
                BindingSemantics::Write(BindingWrite::LookupSlot {
                    name: n,
                    coord,
                    fallback,
                    ..
                }) => {
                    name(n);
                    assert_immediate(schema, coord, ImmediateDomain::ContextCoord);
                    assert_immediate(schema, fallback, ImmediateDomain::StoreFallback);
                }
                BindingSemantics::Read(BindingRead::LookupGlobal { name: n, depth, .. })
                | BindingSemantics::Delete(BindingDelete::LookupSlot { name: n, depth, .. })
                | BindingSemantics::Delete(BindingDelete::LookupGlobal {
                    name: n, depth, ..
                })
                | BindingSemantics::Write(BindingWrite::DeclareEvalVar {
                    name: n,
                    var_depth: depth,
                    ..
                })
                | BindingSemantics::Write(BindingWrite::VarScope {
                    name: n,
                    var_depth: depth,
                    ..
                }) => {
                    name(n);
                    assert_immediate(schema, depth, ImmediateDomain::ContextDepth);
                }
                BindingSemantics::Write(BindingWrite::LookupGlobal { name: n, mode, .. }) => {
                    name(n);
                    assert_immediate(schema, mode, ImmediateDomain::LookupGlobalMode);
                }
                BindingSemantics::Read(BindingRead::ResolveRef {
                    name: n, target, ..
                }) => {
                    name(n);
                    assert_immediate(schema, target, ImmediateDomain::LookupRefTarget);
                }
                BindingSemantics::Write(BindingWrite::StoreRef { name: n, mode, .. }) => {
                    name(n);
                    assert_immediate(schema, mode, ImmediateDomain::StoreRefMode);
                }
            }
        }
    }

    #[test]
    fn binding_register_inputs_name_context_after_value() {
        let context_write = opcode_schema(Op::StoreLookupSlot).binding.unwrap();
        assert_eq!(context_write.value_operands(), [Some(0), Some(1)]);
        assert_eq!(context_write.result_operand(), None);
        let store_ref = opcode_schema(Op::StoreRef).binding.unwrap();
        assert_eq!(store_ref.value_operands(), [Some(0), Some(1)]);
        let context_read = opcode_schema(Op::LoadLookupGlobal).binding.unwrap();
        assert_eq!(context_read.value_operands(), [Some(1), None]);
        assert_eq!(context_read.result_operand(), Some(0));
        let declare = opcode_schema(Op::DeclareEvalVar).binding.unwrap();
        assert_eq!(declare.value_operands(), [Some(0), None]);
        assert_eq!(declare.result_operand(), None);
        let delete = opcode_schema(Op::DeleteLookupSlot).binding.unwrap();
        assert_eq!(delete.value_operands(), [Some(1), None]);
        assert_eq!(delete.result_operand(), Some(0));
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
        assert_eq!(
            operand_spec_at(Op::StoreLookupSlot, 4),
            Some(OperandSpec::immediate(ImmediateDomain::StoreFallback))
        );
        assert_eq!(operand_spec_at(Op::StoreLookupSlot, 5), None);
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
        }
    }
}
