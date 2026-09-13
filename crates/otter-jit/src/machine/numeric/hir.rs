//! Typed scalar HIR and control-flow construction.
//!
//! # Contents
//! - [`NumericFunction`] — bounded numeric SSA graph with explicit blocks.
//! - [`NumericBlock`] and [`NumericTerminator`] — predecessor/successor edges,
//!   block parameters, edge arguments, branches, and returns.
//! - [`NumericNode`] — tagged/scalar parameters, constants, the schema-owned
//!   binding family, guarded coercions, ordinary properties,
//!   indexed elements, arithmetic, comparison, typed array construction, and
//!   typed plain/method calls.
//! - [`NumericPackedDoubleViewCachePlan`] — bounded natural-loop sharing of
//!   raw packed-array base/length proofs.
//!
//! # Invariants
//! - Parameters remain tagged unless their uses prove a numeric representation;
//!   inferred Number/Int32 parameters are guarded before effects.
//! - Empty arithmetic feedback never proves that a site is cold. Such a site
//!   keeps tagged inputs and performs an exact pre-operation numeric decode;
//!   a non-number resumes the canonical bytecode before observable effects.
//!   Later Int32 feedback may narrow a still-tagged input guard, but never an
//!   established Number or Uint32 SSA value.
//! - Tagged values produced inside the function may enter numeric-only regions
//!   through an exact pre-operation guarded decode. A failed decode resumes the
//!   original bytecode before any observable effect can be replayed.
//! - Parameters outside the exact entry live-in set have no HIR value, load,
//!   guard, or allocator interval.
//! - Indexed loads and stores use a baked VM element program plus the GC cage
//!   when one is available. An ordinary packed-double array produces and
//!   consumes unboxed Number values. Its scalar Number index is converted to an
//!   exact Uint32 before the address guard, while a still-tagged index retains
//!   the ordinary exact int32-tag proof. Neither form boxes an already-scalar
//!   payload, and every miss remains an exact pre-effect exit. Other prepared
//!   families retain a distinct fast index and exact boxed key: their fast hit
//!   is direct, while every miss completes once through the same canonical
//!   reentrant `[[Get]]`/`[[Set]]` boundary as an unprepared access. Every
//!   schema-declared may-throw operation inside a local catch must publish its
//!   committed exception SSA value; otherwise the function stays on the
//!   Template baseline.
//!   Every conversion and access frame state describes the exact pre-access
//!   register window.
//! - Ordinary property nodes exist independently of settled shape/slot
//!   metadata. Selection either emits a guarded hit or exact-deoptimizes at the
//!   original bytecode. Named `.length` loads retain their exotic fast-path
//!   marker; every property frame state describes the exact pre-access register
//!   window.
//! - Every schema-owned binding read, write, and delete remains one typed HIR
//!   family. Structurally proven global-this, upvalue, global lexical, and
//!   global-object targets retain a generated sibling; dynamic/shadowed sites
//!   and missing proofs use only the committed cold sibling. Guard miss, TDZ,
//!   const/accessor/Proxy failure, and unresolved lookup never deopt or replay.
//!   The pre-operation frame state exists solely to publish complete tagged
//!   roots at the committed cold safepoint.
//! - Loose numeric equality reuses the guarded numeric path. A tagged value may
//!   compare directly with a static nullish literal, but the node retains an
//!   exact pre-operation state so a native-function cell can deopt for HTMLDDA
//!   semantics.
//! - Literal allocation keeps boxed operand spans and a precise root state;
//!   it cannot invoke JavaScript or replay an allocation after completion.
//! - `ArrayConstruct` accepts only zero arguments or one exact Int32 length.
//!   The allocating operation and any required tagged decode retain the same
//!   exact pre-construction frame state; all wider arities stay on the Template
//!   baseline.
//! - Register reads, writes, and potential implicit exception exits come only
//!   from the bytecode opcode schema. A protected may-throw instruction ends
//!   its HIR block, and its pre-state retains values used only by the innermost
//!   catch. Selection must explicitly return the `NativeResultPair` exception value;
//!   it is never inferred from an operand position.
//! - Reentrant plain/constructor calls remain monomorphic. Guarded methods
//!   accept only a complete dense one-to-four-target VM plan and carry one exact
//!   pre-call FrameState for the whole chain. A never-attempted unplanned plain
//!   or method call becomes an exact pre-effect cold exit. An attempted
//!   unplanned method becomes a zero-candidate direct-method node whose final
//!   miss owns the canonical call; an attempted unplanned plain call keeps the
//!   whole function on the Template baseline. Fixed committed boxed-value and
//!   generated JavaScript calls enter supported catch regions through explicit
//!   exceptional CFG edges that receive the returned pure exception SSA value.
//! - All other tagged coercions/equality use declared leaf stubs; primitive
//!   string concatenation uses the allocating stub family.
//! - Register merges become typed block parameters. Only loop-header OSR
//!   metadata retains the aligned VM-register sources needed at the entry ABI.
//!   Heterogeneous numeric inputs join as Number, while any tagged or Boolean
//!   mixture joins as Tagged. Constructor-transition functions conservatively
//!   retain the Template baseline when such a heterogeneous join is required.
//!   A catch body reached only after an exact deopt is absent from generated
//!   CFG: handler reconstruction resumes it in the interpreter. A committed
//!   pure-exception edge keeps the same catch body reachable in Machine HIR.
//! - Loop headers receive explicit parameters for every numeric value live from
//!   a forward predecessor; backedge arguments are attached after all blocks
//!   are lowered. Headers inside an active exception region remain ordinary
//!   Machine blocks but never publish OSR entry metadata.
//! - Packed-double view caches are planned only for reducible innermost loops
//!   without allocation, JavaScript reentry, or incompatible stores. A cached
//!   receiver is defined outside the loop or passes through an exact identity
//!   header phi; every external entry edge is recorded for mandatory clearing.
//! - HIR preserves source CFG edges; selection splits critical edges before
//!   allocator move placement.
//! - Unprotected static-native calls consume the shared VM leaf declaration,
//!   preserving exact pre-call frame state for identity and operand misses.

use std::collections::{BTreeMap, BTreeSet};

use otter_bytecode::opcode_schema::{
    BindingRead, BindingSemantics, BindingWrite, OperandKind, RegisterAccess, RegisterSource,
    opcode_schema, operand_spec_at,
};
use otter_bytecode::{Op, Operand};
use otter_vm::{JitCompileSnapshot, JitElementBase, JitElementRepr, JitInstructionMetadata};

use super::super::{MAX_PACKED_DOUBLE_VIEW_CACHES, PackedDoubleViewCacheId};
use super::semantics::{
    CommittedValueOperation, InstructionSemantics, classify_snapshot,
    packed_double_element_access_is_exact,
};

const MAX_FUNCTION_INSTRUCTIONS: usize = 512;
const MAX_FUNCTION_PARAMETERS: u16 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct NumericValue(pub(super) usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericType {
    Tagged,
    Int32,
    Uint32,
    Number,
    Boolean,
}

/// Value semantics selected by one immutable element-access snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericElementAccess {
    /// Existing boxed arrays and typed views return/consume tagged values.
    Tagged,
    /// An ordinary Array whose live dense prefix is hole-free raw doubles.
    PackedDouble,
}

/// Structurally safe generated sibling of one binding operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericBindingTarget {
    /// The realm-global object through the snapshot's stable global-this cell.
    GlobalThis,
    /// One stable captured cell in the current native-frame spine.
    Upvalue { index: u32 },
    /// One VM-baked stable global lexical or object target.
    Global(otter_vm::jit::BindingHitProof),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum NumericNode {
    Parameter {
        register: u16,
        value_type: NumericType,
    },
    BlockParameter(NumericType),
    TaggedConstant(u64),
    TaggedToNumber(NumericValue),
    TaggedToInt32(NumericValue),
    This,
    ClassSuperConstructor(NumericValue),
    Binding {
        semantics: BindingSemantics,
        inputs: [Option<NumericValue>; 2],
        target: Option<NumericBindingTarget>,
        logical_pc: u32,
        byte_pc: u32,
        exceptional_edge: Option<u16>,
    },
    StringConstantCell {
        byte_pc: u32,
        target: otter_vm::jit::JitStringConstantCell,
    },
    CommittedValue {
        operation: CommittedValueOperation,
        inputs: [Option<NumericValue>; 2],
        logical_pc: u32,
        byte_pc: u32,
        exceptional_edge: Option<u16>,
    },
    ConstructorFieldStore {
        object: NumericValue,
        value: NumericValue,
        byte_pc: u32,
    },
    PropertyLoad {
        receiver: NumericValue,
        byte_pc: u32,
        exotic_length: bool,
    },
    PropertyStore {
        receiver: NumericValue,
        value: NumericValue,
        byte_pc: u32,
    },
    ElementLoad {
        receiver: NumericValue,
        index: NumericValue,
        byte_pc: u32,
        access: NumericElementAccess,
    },
    ElementStore {
        receiver: NumericValue,
        index: NumericValue,
        value: NumericValue,
        byte_pc: u32,
        access: NumericElementAccess,
    },
    GenericElementLoad {
        receiver: NumericValue,
        index: NumericValue,
        logical_pc: u32,
        byte_pc: u32,
    },
    GenericElementStore {
        receiver: NumericValue,
        index: NumericValue,
        value: NumericValue,
        logical_pc: u32,
        byte_pc: u32,
    },
    CheckedFloat64ToElementIndex {
        value: NumericValue,
        byte_pc: u32,
    },
    LiteralAllocation {
        target: otter_vm::native_abi::RuntimeStubDescriptor,
        argument_start: u32,
        argument_count: u32,
        logical_pc: u32,
        byte_pc: u32,
    },
    ArrayConstruct {
        length: NumericValue,
        byte_pc: u32,
    },
    TaggedToBoolean(NumericValue),
    TaggedStrictEqual(NumericValue, NumericValue),
    TaggedNullishEqual {
        value: NumericValue,
        equal: bool,
        byte_pc: u32,
    },
    TaggedStringConcat(NumericValue, NumericValue),
    DirectCall {
        source: NumericValue,
        target: u16,
        arguments: NumericDirectCallArguments,
        logical_pc: u32,
        byte_pc: u32,
        exceptional_edge: Option<u16>,
    },
    NativeLeaf {
        source: NumericValue,
        target: otter_vm::JitStaticNativeCall,
        value_type: NumericType,
        argument_start: u32,
        byte_pc: u32,
    },
    ColdCallExit {
        kind: NumericColdCallKind,
        logical_pc: u32,
        byte_pc: u32,
        exceptional_edge: Option<u16>,
    },
    IntegerConstant(i32),
    BooleanConstant(bool),
    Constant(f64),
    WidenInt32(NumericValue),
    WidenUint32(NumericValue),
    FloatToInt32(NumericValue),
    BooleanToInt32(NumericValue),
    IntegerAdd(NumericValue, NumericValue),
    IntegerSub(NumericValue, NumericValue),
    IntegerMul(NumericValue, NumericValue),
    IntegerNeg(NumericValue),
    IntegerAddImmediate(NumericValue, i32),
    IntegerSubImmediate(NumericValue, i32),
    IntegerAnd(NumericValue, NumericValue),
    IntegerOr(NumericValue, NumericValue),
    IntegerXor(NumericValue, NumericValue),
    IntegerShiftLeft(NumericValue, NumericValue),
    IntegerShiftRight(NumericValue, NumericValue),
    IntegerShiftRightLogical(NumericValue, NumericValue),
    IntegerNot(NumericValue),
    IntegerAndImmediate(NumericValue, i32),
    IntegerLessThanImmediate(NumericValue, i32),
    IntegerEqualImmediate(NumericValue, i32),
    IntegerNotEqualImmediate(NumericValue, i32),
    IntegerEqual(NumericValue, NumericValue),
    IntegerNotEqual(NumericValue, NumericValue),
    IntegerLessThan(NumericValue, NumericValue),
    IntegerLessEqual(NumericValue, NumericValue),
    IntegerGreaterThan(NumericValue, NumericValue),
    IntegerGreaterEqual(NumericValue, NumericValue),
    Add(NumericValue, NumericValue),
    Sub(NumericValue, NumericValue),
    Mul(NumericValue, NumericValue),
    Div(NumericValue, NumericValue),
    Rem(NumericValue, NumericValue),
    Pow(NumericValue, NumericValue),
    Neg(NumericValue),
    IntegerToBoolean(NumericValue),
    FloatToBoolean(NumericValue),
    BooleanNot(NumericValue),
    LessThan(NumericValue, NumericValue),
    Equal(NumericValue, NumericValue),
    NotEqual(NumericValue, NumericValue),
    LessEqual(NumericValue, NumericValue),
    GreaterThan(NumericValue, NumericValue),
    GreaterEqual(NumericValue, NumericValue),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum NumericDirectCallArguments {
    Fixed { start: u32, count: u32 },
    Spread(NumericValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericDirectCallKind {
    Plain,
    /// Explicit receiver in argument word zero; at most one identity-guarded
    /// candidate, otherwise the generic value call.
    CallWithThis,
    Method,
    Construct,
    DerivedConstruct,
    SuperConstruct,
    DerivedSuperConstruct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericColdCallKind {
    Plain,
    Method,
}

/// Why one function stayed outside the Machine HIR.
///
/// The first decline observed wins: a structural rule names itself, and an
/// instruction the lowering rejected names its opcode, logical PC, and the
/// condition that failed, so compile diagnostics attribute every refusal to
/// something a reader can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HirDecline {
    Structural(&'static str),
    Instruction {
        op: Op,
        logical_pc: u32,
        constraint: &'static str,
    },
}

fn note_decline(
    decline: &mut Option<HirDecline>,
    op: Op,
    logical_pc: u32,
    constraint: &'static str,
) {
    decline.get_or_insert(HirDecline::Instruction {
        op,
        logical_pc,
        constraint,
    });
}

fn note_structural(decline: &mut Option<HirDecline>, constraint: &'static str) {
    decline.get_or_insert(HirDecline::Structural(constraint));
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericDirectCallCandidate {
    pub(super) target_index: u32,
    pub(super) target_count: u32,
    pub(super) guard: Option<otter_vm::jit::JitMethodGuard>,
    pub(super) callee: otter_vm::JitDirectCallee,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericDirectCallTarget {
    pub(super) kind: NumericDirectCallKind,
    pub(super) candidates: Vec<NumericDirectCallCandidate>,
}

fn monomorphic_direct_call_target(
    kind: NumericDirectCallKind,
    callee: otter_vm::JitDirectCallee,
) -> NumericDirectCallTarget {
    NumericDirectCallTarget {
        kind,
        candidates: vec![NumericDirectCallCandidate {
            target_index: 0,
            target_count: 1,
            guard: None,
            callee,
        }],
    }
}

fn method_direct_call_target(
    methods: &[otter_vm::jit::JitDirectMethod],
) -> Option<NumericDirectCallTarget> {
    let target_count = u32::try_from(methods.len()).ok()?;
    if !(1..=crate::machine::MAX_MACHINE_DIRECT_METHOD_TARGETS).contains(&methods.len()) {
        return None;
    }
    let mut candidates = Vec::with_capacity(methods.len());
    for (target_index, method) in methods.iter().enumerate() {
        if method.target_index != u32::try_from(target_index).ok()?
            || method.target_count != target_count
            || method.guard.method_fid != method.callee.plan.function_id
        {
            return None;
        }
        candidates.push(NumericDirectCallCandidate {
            target_index: method.target_index,
            target_count: method.target_count,
            guard: Some(method.guard.clone()),
            callee: method.callee,
        });
    }
    Some(NumericDirectCallTarget {
        kind: NumericDirectCallKind::Method,
        candidates,
    })
}

fn generic_method_call_target() -> NumericDirectCallTarget {
    NumericDirectCallTarget {
        kind: NumericDirectCallKind::Method,
        candidates: Vec::new(),
    }
}

fn generic_call_with_this_target() -> NumericDirectCallTarget {
    NumericDirectCallTarget {
        kind: NumericDirectCallKind::CallWithThis,
        candidates: Vec::new(),
    }
}

fn append_operand_values(
    storage: &mut Vec<NumericValue>,
    arguments: impl IntoIterator<Item = NumericValue>,
) -> Option<(u32, u32)> {
    let start = u32::try_from(storage.len()).ok()?;
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    let count = u32::try_from(arguments.len()).ok()?;
    start.checked_add(count)?;
    storage.extend(arguments);
    Some((start, count))
}

fn intern_direct_call_target(
    targets: &mut Vec<NumericDirectCallTarget>,
    target: NumericDirectCallTarget,
) -> Option<u16> {
    let index = targets
        .iter()
        .position(|candidate| *candidate == target)
        .unwrap_or_else(|| {
            targets.push(target);
            targets.len() - 1
        });
    u16::try_from(index).ok()
}

#[allow(clippy::too_many_arguments)]
fn lower_cold_call_exit(
    kind: NumericColdCallKind,
    destination: u16,
    logical_pc: u32,
    byte_pc: u32,
    exceptional_edge: Option<usize>,
    registers: &mut [RegisterState],
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    function_id: u32,
    live_in: &[bool],
    exceptional_value: &mut Option<NumericValue>,
) -> Option<()> {
    let value = push(
        nodes,
        NumericNode::ColdCallExit {
            kind,
            logical_pc,
            byte_pc,
            exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
        },
    );
    block_nodes.push(value);
    push_frame_state(
        frame_states,
        NumericFramePoint::Node(value),
        function_id,
        byte_pc,
        registers,
        live_in,
    );
    record_exceptional_value(exceptional_value, exceptional_edge, value)?;
    write(registers, destination, RegisterState::Value(value))
}

fn record_exceptional_value(
    exceptional_value: &mut Option<NumericValue>,
    exceptional_edge: Option<usize>,
    value: NumericValue,
) -> Option<()> {
    if exceptional_edge.is_some() && exceptional_value.replace(value).is_some() {
        return None;
    }
    Some(())
}

impl NumericNode {
    pub(super) const fn value_type(self) -> NumericType {
        match self {
            Self::TaggedConstant(..)
            | Self::This
            | Self::ClassSuperConstructor(..)
            | Self::Binding { .. }
            | Self::StringConstantCell { .. }
            | Self::CommittedValue { .. }
            | Self::ConstructorFieldStore { .. }
            | Self::PropertyLoad { .. }
            | Self::PropertyStore { .. }
            | Self::ElementStore { .. }
            | Self::GenericElementLoad { .. }
            | Self::GenericElementStore { .. }
            | Self::ArrayConstruct { .. }
            | Self::LiteralAllocation { .. }
            | Self::TaggedStringConcat(..)
            | Self::DirectCall { .. }
            | Self::ColdCallExit { .. }
            | Self::BlockParameter(NumericType::Tagged) => NumericType::Tagged,
            Self::ElementLoad {
                access: NumericElementAccess::Tagged,
                ..
            } => NumericType::Tagged,
            Self::IntegerConstant(..)
            | Self::TaggedToInt32(..)
            | Self::FloatToInt32(..)
            | Self::BooleanToInt32(..)
            | Self::IntegerAdd(..)
            | Self::IntegerSub(..)
            | Self::IntegerMul(..)
            | Self::IntegerNeg(..)
            | Self::IntegerAddImmediate(..)
            | Self::IntegerSubImmediate(..)
            | Self::IntegerAnd(..)
            | Self::IntegerOr(..)
            | Self::IntegerXor(..)
            | Self::IntegerShiftLeft(..)
            | Self::IntegerShiftRight(..)
            | Self::IntegerNot(..)
            | Self::IntegerAndImmediate(..)
            | Self::BlockParameter(NumericType::Int32) => NumericType::Int32,
            Self::IntegerShiftRightLogical(..)
            | Self::CheckedFloat64ToElementIndex { .. }
            | Self::BlockParameter(NumericType::Uint32) => NumericType::Uint32,
            Self::LessThan(..)
            | Self::Equal(..)
            | Self::NotEqual(..)
            | Self::LessEqual(..)
            | Self::GreaterThan(..)
            | Self::GreaterEqual(..)
            | Self::IntegerEqual(..)
            | Self::IntegerNotEqual(..)
            | Self::IntegerLessThan(..)
            | Self::IntegerLessEqual(..)
            | Self::IntegerGreaterThan(..)
            | Self::IntegerGreaterEqual(..)
            | Self::TaggedToBoolean(..)
            | Self::TaggedStrictEqual(..)
            | Self::TaggedNullishEqual { .. }
            | Self::IntegerToBoolean(..)
            | Self::FloatToBoolean(..)
            | Self::BooleanNot(..)
            | Self::IntegerLessThanImmediate(..)
            | Self::IntegerEqualImmediate(..)
            | Self::IntegerNotEqualImmediate(..)
            | Self::BlockParameter(NumericType::Boolean) => NumericType::Boolean,
            Self::BooleanConstant(..) => NumericType::Boolean,
            Self::Parameter { value_type, .. } | Self::NativeLeaf { value_type, .. } => value_type,
            Self::BlockParameter(NumericType::Number)
            | Self::ElementLoad {
                access: NumericElementAccess::PackedDouble,
                ..
            }
            | Self::TaggedToNumber(..)
            | Self::Constant(..)
            | Self::WidenInt32(..)
            | Self::WidenUint32(..)
            | Self::Add(..)
            | Self::Sub(..)
            | Self::Mul(..)
            | Self::Div(..)
            | Self::Rem(..)
            | Self::Pow(..)
            | Self::Neg(..) => NumericType::Number,
        }
    }

    /// Authoritative selection use of an attached frame state.
    pub(super) const fn frame_state_purpose(self) -> Option<NumericFrameStatePurpose> {
        match self {
            Self::CommittedValue { .. } | Self::Binding { .. } | Self::LiteralAllocation { .. } => {
                Some(NumericFrameStatePurpose::TaggedRoots)
            }
            Self::GenericElementLoad { .. } | Self::GenericElementStore { .. } => {
                Some(NumericFrameStatePurpose::RuntimeMetadata)
            }
            Self::ColdCallExit { .. }
            | Self::TaggedToNumber(..)
            | Self::TaggedToInt32(..)
            | Self::ClassSuperConstructor(..)
            | Self::ConstructorFieldStore { .. }
            | Self::PropertyLoad { .. }
            | Self::PropertyStore { .. }
            | Self::ElementLoad { .. }
            | Self::ElementStore { .. }
            | Self::CheckedFloat64ToElementIndex { .. }
            | Self::ArrayConstruct { .. }
            | Self::DirectCall { .. }
            | Self::NativeLeaf { .. }
            | Self::TaggedToBoolean(..)
            | Self::TaggedStrictEqual(..)
            | Self::TaggedNullishEqual { .. }
            | Self::TaggedStringConcat(..)
            | Self::IntegerAdd(..)
            | Self::IntegerSub(..)
            | Self::IntegerMul(..)
            | Self::IntegerNeg(..)
            | Self::IntegerAddImmediate(..)
            | Self::IntegerSubImmediate(..) => Some(NumericFrameStatePurpose::ExactDeopt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterState {
    Unset,
    Value(NumericValue),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericTerminator {
    Jump,
    Branch {
        condition: NumericValue,
        when_true: bool,
    },
    Return(NumericValue),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericBlock {
    pub(super) logical_pc: u32,
    /// Whether selection may publish an OSR trampoline at this block.
    pub(super) osr_entry_allowed: bool,
    pub(super) predecessors: Vec<usize>,
    pub(super) successors: Vec<usize>,
    pub(super) parameters: Vec<NumericValue>,
    pub(super) parameter_registers: Vec<u16>,
    pub(super) successor_arguments: Vec<Vec<NumericValue>>,
    pub(super) nodes: Vec<NumericValue>,
    pub(super) terminator: NumericTerminator,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NumericFunction {
    pub(super) function_id: u32,
    pub(super) nodes: Vec<NumericNode>,
    pub(super) blocks: Vec<NumericBlock>,
    pub(super) frame_states: Vec<NumericFrameState>,
    pub(super) direct_call_targets: Vec<NumericDirectCallTarget>,
    /// Shared immutable spans for call, native-leaf and literal operands.
    pub(super) operand_values: Vec<NumericValue>,
    pub(super) parameter_count: u16,
    pub(super) register_count: u16,
    pub(super) arithmetic_op_count: usize,
}

/// One proven loop-scoped packed-double view shared by element sites.
#[derive(Debug, Clone)]
pub(super) struct NumericPackedDoubleViewCache {
    /// Dense bounded identity used to address two raw frame words.
    pub(super) id: PackedDoubleViewCacheId,
    /// Reducible natural-loop header whose backedges may retain the view.
    pub(super) loop_header: usize,
    /// Canonical receiver value before any identity loop phi.
    pub(super) receiver_root: NumericValue,
    /// Outside-to-header edges that must clear this cache before entry.
    pub(super) entry_edges: BTreeSet<(usize, usize)>,
    /// Complete immutable VM layout proof shared by every grouped site.
    pub(super) access: otter_vm::JitElementAccess,
}

/// Conservative target-neutral packed-double view-cache plan.
#[derive(Debug, Clone, Default)]
pub(super) struct NumericPackedDoubleViewCachePlan {
    /// Cache descriptors in deterministic dense-id order.
    pub(super) caches: Vec<NumericPackedDoubleViewCache>,
    /// Element HIR value to its shared cache identity.
    pub(super) sites: BTreeMap<NumericValue, PackedDoubleViewCacheId>,
}

impl NumericPackedDoubleViewCachePlan {
    /// Cache identity assigned to one packed-double load or store.
    #[must_use]
    pub(super) fn cache_for(&self, site: NumericValue) -> Option<PackedDoubleViewCacheId> {
        self.sites.get(&site).copied()
    }
}

#[derive(Debug, Clone)]
struct NumericNaturalLoop {
    header: usize,
    blocks: BTreeSet<usize>,
}

type PhiTypeOverrides = BTreeMap<(usize, u16), NumericType>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericFrameSlot {
    Value(NumericValue),
    Undefined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NumericFrameState {
    pub(super) point: NumericFramePoint,
    pub(super) function_id: u32,
    pub(super) byte_pc: u32,
    pub(super) slots: Vec<NumericFrameSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum NumericFramePoint {
    Node(NumericValue),
    Backedge { predecessor: usize, edge: usize },
}

/// How selection consumes a HIR frame state attached to one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumericFrameStatePurpose {
    /// A reachable pre-effect exact-deopt exit reconstructs the VM window.
    ExactDeopt,
    /// A committed effect needs complete live tagged roots but no deopt.
    TaggedRoots,
    /// An effect-once emitter uses the state only to publish source identity.
    RuntimeMetadata,
}

#[derive(Debug, Clone, Copy)]
enum RawTerminator {
    Jump,
    Branch { when_true: bool },
    ReturnValue,
    ReturnUndefined,
}

#[derive(Debug, Clone)]
struct RawBlock {
    start: usize,
    end: usize,
    predecessors: Vec<usize>,
    successors: Vec<usize>,
    exceptional_edge: Option<usize>,
    exception_register: Option<u16>,
    terminator: RawTerminator,
}

#[derive(Debug, Clone, Copy)]
struct InstructionExceptionHandler {
    block: usize,
    exception_register: u16,
}

impl NumericFunction {
    pub(super) fn build(view: &JitCompileSnapshot) -> Result<Self, HirDecline> {
        let mut phi_types = PhiTypeOverrides::new();
        let mut decline = None;
        for _ in 0..MAX_FUNCTION_INSTRUCTIONS {
            let Some((function, next_phi_types, retry)) =
                Self::build_attempt(view, &phi_types, &mut decline)
            else {
                return Err(decline.unwrap_or(HirDecline::Structural("Machine HIR lowering")));
            };
            if !retry {
                return Ok(function);
            }
            if next_phi_types == phi_types {
                return Err(HirDecline::Structural(
                    "phi representations did not converge",
                ));
            }
            phi_types = next_phi_types;
        }
        Err(HirDecline::Structural(
            "phi representation retries exhausted",
        ))
    }

    /// Plan bounded raw view caches for safe packed-double loop sites.
    ///
    /// This analysis deliberately runs after HIR construction, when receiver
    /// identity phis and the complete CFG are explicit. Failure to prove one
    /// site merely leaves that site on its ordinary per-access guard path.
    pub(super) fn plan_packed_double_view_caches(
        &self,
        view: &JitCompileSnapshot,
    ) -> NumericPackedDoubleViewCachePlan {
        let Some(loops) = innermost_reducible_natural_loops(&self.blocks) else {
            return NumericPackedDoubleViewCachePlan::default();
        };
        let Some(node_blocks) = numeric_node_blocks(self) else {
            return NumericPackedDoubleViewCachePlan::default();
        };
        let mut plan = NumericPackedDoubleViewCachePlan::default();

        for natural_loop in loops {
            if !packed_double_cache_loop_is_safe(self, &natural_loop) {
                continue;
            }
            for &block_index in &natural_loop.blocks {
                let Some(block) = self.blocks.get(block_index) else {
                    continue;
                };
                for &site in &block.nodes {
                    let Some((receiver, byte_pc)) =
                        self.nodes.get(site.0).and_then(|node| match *node {
                            NumericNode::ElementLoad {
                                receiver,
                                byte_pc,
                                access: NumericElementAccess::PackedDouble,
                                ..
                            }
                            | NumericNode::ElementStore {
                                receiver,
                                byte_pc,
                                access: NumericElementAccess::PackedDouble,
                                ..
                            } => Some((receiver, byte_pc)),
                            _ => None,
                        })
                    else {
                        continue;
                    };
                    let Some(access) = view.element_accesses.get(&byte_pc).copied() else {
                        continue;
                    };
                    if !packed_double_element_access_is_exact(&access) {
                        continue;
                    }
                    let Some(receiver_root) = canonical_loop_invariant_receiver(
                        self,
                        &natural_loop,
                        &node_blocks,
                        receiver,
                    ) else {
                        continue;
                    };

                    let cache_id = plan
                        .caches
                        .iter()
                        .find(|cache| {
                            cache.loop_header == natural_loop.header
                                && cache.receiver_root == receiver_root
                                && same_element_access(&cache.access, &access)
                        })
                        .map(|cache| cache.id)
                        .or_else(|| {
                            let id = PackedDoubleViewCacheId::new(plan.caches.len())?;
                            plan.caches.push(NumericPackedDoubleViewCache {
                                id,
                                loop_header: natural_loop.header,
                                receiver_root,
                                entry_edges: packed_double_cache_entry_edges(
                                    &self.blocks,
                                    &natural_loop,
                                ),
                                access,
                            });
                            Some(id)
                        });
                    if let Some(cache_id) = cache_id {
                        plan.sites.insert(site, cache_id);
                    }
                }
            }
        }

        debug_assert!(plan.caches.len() <= MAX_PACKED_DOUBLE_VIEW_CACHES);
        plan
    }

    fn build_attempt(
        view: &JitCompileSnapshot,
        phi_types: &PhiTypeOverrides,
        decline: &mut Option<HirDecline>,
    ) -> Option<(Self, PhiTypeOverrides, bool)> {
        let code = view.code_block.as_ref();
        let parameter_count = code.param_count;
        let register_count = code.register_count;
        if code.is_async || code.is_generator || code.is_async_generator {
            note_structural(decline, "suspendable function");
            return None;
        }
        if parameter_count > register_count
            || parameter_count > MAX_FUNCTION_PARAMETERS
            || view.instructions.is_empty()
            || view.instructions.len() > MAX_FUNCTION_INSTRUCTIONS
        {
            note_structural(decline, "function size outside the Machine bounds");
            return None;
        }
        if code
            .control_flow()
            .exception_regions()
            .iter()
            .any(|region| region.catch_pc.is_none() || region.finally_pc.is_some())
        {
            note_structural(decline, "finally or catch-less exception region");
            return None;
        }

        let Some(instruction_semantics) = classify_snapshot(view) else {
            note_structural(decline, "instruction semantics");
            return None;
        };
        let Some(raw_blocks) = build_raw_blocks(view, &instruction_semantics) else {
            note_structural(decline, "reducible control flow");
            return None;
        };
        let Some(parameter_types) =
            infer_parameter_types(view, &raw_blocks, parameter_count, register_count)
        else {
            note_structural(decline, "parameter representations");
            return None;
        };
        let Some(exception_handlers) =
            build_instruction_exception_handlers(view, &raw_blocks, &instruction_semantics)
        else {
            note_structural(decline, "exception handler layout");
            return None;
        };
        let live_in = build_liveness(
            view,
            &raw_blocks,
            &exception_handlers,
            &instruction_semantics,
            register_count,
        )?;
        let instruction_live_in = build_instruction_liveness(
            view,
            &raw_blocks,
            &live_in,
            &exception_handlers,
            &instruction_semantics,
            register_count,
        )?;
        let mut nodes = Vec::with_capacity(view.instructions.len() + register_count as usize);
        let mut entry = vec![RegisterState::Unset; usize::from(register_count)];
        let mut entry_nodes = Vec::with_capacity(parameter_count as usize);
        for parameter in 0..parameter_count {
            if !live_in[0][usize::from(parameter)] {
                continue;
            }
            let value = push(
                &mut nodes,
                NumericNode::Parameter {
                    register: parameter,
                    value_type: parameter_types[usize::from(parameter)],
                },
            );
            entry[usize::from(parameter)] = RegisterState::Value(value);
            entry_nodes.push(value);
        }

        let mut blocks = Vec::with_capacity(raw_blocks.len());
        let mut out_states = Vec::<Vec<RegisterState>>::with_capacity(raw_blocks.len());
        let mut exceptional_out_states =
            Vec::<Option<Vec<RegisterState>>>::with_capacity(raw_blocks.len());
        let mut arithmetic_op_count = 0usize;
        let mut frame_states = Vec::new();
        let mut direct_call_targets = Vec::new();
        let mut operand_values = Vec::new();
        let mut requires_mixed_join = false;

        for (block_index, raw) in raw_blocks.iter().enumerate() {
            let (mut registers, mut parameters, mut parameter_regs) = if block_index == 0 {
                (entry.clone(), Vec::new(), Vec::new())
            } else if raw
                .predecessors
                .iter()
                .any(|&predecessor| predecessor >= block_index)
            {
                let forward_predecessors = raw
                    .predecessors
                    .iter()
                    .copied()
                    .filter(|&predecessor| predecessor < block_index)
                    .collect::<Vec<_>>();
                merge_predecessors(
                    &forward_predecessors,
                    block_index,
                    &raw_blocks,
                    &live_in[block_index],
                    &out_states,
                    &exceptional_out_states,
                    &mut nodes,
                    phi_types,
                    &mut requires_mixed_join,
                )?
            } else {
                merge_predecessors(
                    &raw.predecessors,
                    block_index,
                    &raw_blocks,
                    &live_in[block_index],
                    &out_states,
                    &exceptional_out_states,
                    &mut nodes,
                    phi_types,
                    &mut requires_mixed_join,
                )?
            };
            if raw
                .predecessors
                .iter()
                .any(|&predecessor| predecessor >= block_index)
            {
                force_loop_parameters(
                    &mut registers,
                    &mut parameters,
                    &mut parameter_regs,
                    &mut nodes,
                    &live_in[block_index],
                    block_index,
                    phi_types,
                    &mut requires_mixed_join,
                )?;
            }
            force_exception_parameters(
                block_index,
                &raw_blocks,
                &mut registers,
                &mut parameters,
                &mut parameter_regs,
                &mut nodes,
            )?;
            let mut block_nodes = if block_index == 0 {
                entry_nodes.clone()
            } else {
                Vec::new()
            };
            block_nodes.extend(parameters.iter().copied());
            let terminal_pc = raw.end.checked_sub(1)?;
            let mut exceptional_pre_state = None;
            let mut exceptional_value = None;
            for (pc, instruction_live) in instruction_live_in
                .iter()
                .enumerate()
                .take(raw.end)
                .skip(raw.start)
            {
                let instruction = &view.instructions[pc];
                let op = instruction.op(code);
                if pc == terminal_pc
                    && matches!(
                        op,
                        Op::Jump
                            | Op::JumpIfTrue
                            | Op::JumpIfFalse
                            | Op::Return
                            | Op::ReturnValue
                            | Op::ReturnUndefined
                    )
                {
                    break;
                }
                if pc == terminal_pc && raw.exceptional_edge.is_some() {
                    exceptional_pre_state = Some(registers.clone());
                }
                let lowered = lower_instruction(
                    decline,
                    instruction,
                    code,
                    view.derived_constructor,
                    &mut registers,
                    &mut nodes,
                    &mut block_nodes,
                    &mut arithmetic_op_count,
                    instruction_live,
                    &mut frame_states,
                    code.id,
                    u32::try_from(pc).ok()?,
                    &view.direct_callees,
                    &view.direct_constructs,
                    &view.direct_methods,
                    &view.constructor_field_transitions,
                    &view.element_accesses,
                    &view.string_constant_cells,
                    &view.binding_hit_proofs,
                    view,
                    &mut direct_call_targets,
                    &mut operand_values,
                    instruction_semantics[pc],
                    (pc == terminal_pc)
                        .then_some(raw.exceptional_edge)
                        .flatten(),
                    &mut exceptional_value,
                );
                if lowered.is_none() {
                    note_decline(
                        decline,
                        instruction.op(code),
                        u32::try_from(pc).ok()?,
                        "lowering rejected the site",
                    );
                    return None;
                }
            }

            let terminal = &view.instructions[terminal_pc];
            let terminator = match raw.terminator {
                RawTerminator::Jump => NumericTerminator::Jump,
                RawTerminator::Branch { when_true } => {
                    let source = read_value(&registers, register(terminal, code, 1)?)?;
                    let condition = to_boolean(source, &mut nodes, &mut block_nodes)?;
                    if matches!(nodes[condition.0], NumericNode::TaggedToBoolean(..)) {
                        push_frame_state(
                            &mut frame_states,
                            NumericFramePoint::Node(condition),
                            code.id,
                            terminal.byte_pc,
                            &registers,
                            &instruction_live_in[terminal_pc],
                        );
                    }
                    NumericTerminator::Branch {
                        condition,
                        when_true,
                    }
                }
                RawTerminator::ReturnValue => {
                    NumericTerminator::Return(read_value(&registers, register(terminal, code, 0)?)?)
                }
                RawTerminator::ReturnUndefined => {
                    let value = push(
                        &mut nodes,
                        NumericNode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                    );
                    block_nodes.push(value);
                    NumericTerminator::Return(value)
                }
            };

            let exceptional_state = match (
                raw.exceptional_edge,
                exceptional_pre_state,
                raw.exception_register,
                exceptional_value,
            ) {
                (None, None, None, None) => None,
                (Some(_), Some(mut state), Some(exception_register), Some(value)) => {
                    state[usize::from(exception_register)] = RegisterState::Value(value);
                    Some(state)
                }
                // Schema establishes the potential catch edge. Selection must
                // explicitly publish the `NativeResultPair` exception value; guessing
                // it from operand zero aliases stores and other non-result
                // operations with an unrelated receiver.
                (Some(_), Some(_), Some(_), None) => return None,
                _ => return None,
            };
            out_states.push(registers);
            exceptional_out_states.push(exceptional_state);
            blocks.push(NumericBlock {
                logical_pc: u32::try_from(raw.start).ok()?,
                osr_entry_allowed: code
                    .control_flow()
                    .enclosing_exception_region(u32::try_from(raw.start).ok()?)
                    .is_none(),
                predecessors: raw.predecessors.clone(),
                successors: raw.successors.clone(),
                parameters,
                parameter_registers: parameter_regs,
                successor_arguments: vec![Vec::new(); raw.successors.len()],
                nodes: block_nodes,
                terminator,
            });
        }

        let mut next_phi_types = phi_types.clone();
        let mut retry = false;
        for predecessor in 0..blocks.len() {
            for edge in 0..blocks[predecessor].successors.len() {
                let successor = blocks[predecessor].successors[edge];
                let successor_registers = blocks[successor].parameter_registers.clone();
                let edge_state = edge_state(
                    predecessor,
                    edge,
                    &raw_blocks,
                    &out_states,
                    &exceptional_out_states,
                )?;
                let arguments = successor_registers
                    .iter()
                    .map(|&register| match edge_state[usize::from(register)] {
                        RegisterState::Value(value) => Some(value),
                        RegisterState::Unset => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                for ((&argument, &parameter), &register) in arguments
                    .iter()
                    .zip(&blocks[successor].parameters)
                    .zip(&successor_registers)
                {
                    let argument_type = value_type(&nodes, argument)?;
                    let parameter_type = value_type(&nodes, parameter)?;
                    if argument_type == parameter_type {
                        continue;
                    }
                    requires_mixed_join = true;
                    let joined = join_representation_types(parameter_type, argument_type)?;
                    if joined != parameter_type {
                        let key = (successor, register);
                        let requested = next_phi_types.get(&key).copied().unwrap_or(parameter_type);
                        let requested = join_representation_types(requested, joined)?;
                        if requested != parameter_type {
                            next_phi_types.insert(key, requested);
                            retry = true;
                        }
                    }
                }
                blocks[predecessor].successor_arguments[edge] = arguments;
            }
        }

        if requires_mixed_join && !view.constructor_field_transitions.is_empty() {
            note_structural(
                decline,
                "mixed representation join inside a constructor transition program",
            );
            return None;
        }

        for (predecessor, block) in blocks.iter().enumerate() {
            for (edge, &successor) in block.successors.iter().enumerate() {
                if successor > predecessor {
                    continue;
                }
                frame_states.push(NumericFrameState {
                    point: NumericFramePoint::Backedge { predecessor, edge },
                    function_id: code.id,
                    byte_pc: view.instructions.get(raw_blocks[successor].start)?.byte_pc,
                    slots: out_states[predecessor]
                        .iter()
                        .copied()
                        .zip(live_in[successor].iter().copied())
                        .map(|(state, live)| match (state, live) {
                            (RegisterState::Value(value), true) => NumericFrameSlot::Value(value),
                            (RegisterState::Unset, _) | (RegisterState::Value(_), false) => {
                                NumericFrameSlot::Undefined
                            }
                        })
                        .collect(),
                });
            }
        }

        Some((
            Self {
                function_id: code.id,
                nodes,
                blocks,
                frame_states,
                direct_call_targets,
                operand_values,
                parameter_count,
                register_count,
                arithmetic_op_count,
            },
            next_phi_types,
            retry,
        ))
    }
}

fn numeric_node_blocks(function: &NumericFunction) -> Option<Vec<Option<usize>>> {
    let mut blocks = vec![None; function.nodes.len()];
    for (block_index, block) in function.blocks.iter().enumerate() {
        for &node in &block.nodes {
            let owner = blocks.get_mut(node.0)?;
            match *owner {
                None => *owner = Some(block_index),
                Some(previous) if previous == block_index => {}
                Some(_) => return None,
            }
        }
    }
    Some(blocks)
}

fn innermost_reducible_natural_loops(blocks: &[NumericBlock]) -> Option<Vec<NumericNaturalLoop>> {
    let dominators = numeric_dominators(blocks)?;
    let mut loops = Vec::<NumericNaturalLoop>::new();
    for (latch, block) in blocks.iter().enumerate() {
        for &header in &block.successors {
            if !dominators.get(latch)?.contains(&header) {
                continue;
            }
            let mut members = BTreeSet::from([header, latch]);
            let mut pending = (latch != header)
                .then_some(latch)
                .into_iter()
                .collect::<Vec<_>>();
            while let Some(member) = pending.pop() {
                for &predecessor in &blocks.get(member)?.predecessors {
                    if members.insert(predecessor) && predecessor != header {
                        pending.push(predecessor);
                    }
                }
            }
            if members.iter().any(|&member| {
                member != header
                    && blocks[member]
                        .predecessors
                        .iter()
                        .any(|predecessor| !members.contains(predecessor))
            }) {
                continue;
            }
            if let Some(existing) = loops
                .iter_mut()
                .find(|candidate| candidate.header == header)
            {
                existing.blocks.extend(members);
            } else {
                loops.push(NumericNaturalLoop {
                    header,
                    blocks: members,
                });
            }
        }
    }

    let all_loops = loops.clone();
    loops.retain(|candidate| {
        !all_loops.iter().any(|inner| {
            inner.header != candidate.header
                && inner.blocks.len() < candidate.blocks.len()
                && inner.blocks.is_subset(&candidate.blocks)
        })
    });
    loops.sort_by_key(|natural_loop| natural_loop.header);
    Some(loops)
}

fn numeric_dominators(blocks: &[NumericBlock]) -> Option<Vec<BTreeSet<usize>>> {
    if blocks.is_empty() {
        return None;
    }
    let all = (0..blocks.len()).collect::<BTreeSet<_>>();
    let mut dominators = vec![all; blocks.len()];
    dominators[0] = BTreeSet::from([0]);
    loop {
        let mut changed = false;
        for block_index in 1..blocks.len() {
            let block = blocks.get(block_index)?;
            let mut next = block
                .predecessors
                .iter()
                .map(|&predecessor| dominators.get(predecessor).cloned())
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .reduce(|left, right| left.intersection(&right).copied().collect())?;
            next.insert(block_index);
            if next != dominators[block_index] {
                dominators[block_index] = next;
                changed = true;
            }
        }
        if !changed {
            return Some(dominators);
        }
    }
}

fn packed_double_cache_loop_is_safe(
    function: &NumericFunction,
    natural_loop: &NumericNaturalLoop,
) -> bool {
    natural_loop.blocks.iter().all(|&block_index| {
        function.blocks.get(block_index).is_some_and(|block| {
            block.nodes.iter().all(|&node| {
                function.nodes.get(node.0).is_some_and(|node| {
                    !matches!(
                        node,
                        NumericNode::Binding { .. }
                            | NumericNode::DirectCall { .. }
                            | NumericNode::ColdCallExit { .. }
                            | NumericNode::ArrayConstruct { .. }
                            | NumericNode::LiteralAllocation { .. }
                            | NumericNode::TaggedStringConcat(..)
                            | NumericNode::CommittedValue { .. }
                            | NumericNode::ClassSuperConstructor(..)
                            | NumericNode::ConstructorFieldStore { .. }
                            | NumericNode::PropertyStore { .. }
                            | NumericNode::ElementStore {
                                access: NumericElementAccess::Tagged,
                                ..
                            }
                    )
                })
            })
        })
    })
}

fn packed_double_cache_entry_edges(
    blocks: &[NumericBlock],
    natural_loop: &NumericNaturalLoop,
) -> BTreeSet<(usize, usize)> {
    blocks
        .iter()
        .enumerate()
        .filter(|(predecessor, _)| !natural_loop.blocks.contains(predecessor))
        .flat_map(|(predecessor, block)| {
            block
                .successors
                .iter()
                .enumerate()
                .filter_map(move |(edge, &successor)| {
                    (successor == natural_loop.header).then_some((predecessor, edge))
                })
        })
        .collect()
}

fn canonical_loop_invariant_receiver(
    function: &NumericFunction,
    natural_loop: &NumericNaturalLoop,
    node_blocks: &[Option<usize>],
    receiver: NumericValue,
) -> Option<NumericValue> {
    let header = function.blocks.get(natural_loop.header)?;
    if let Some(parameter_index) = header
        .parameters
        .iter()
        .position(|&parameter| parameter == receiver)
    {
        let mut root = None;
        for &predecessor in &header.predecessors {
            let edge = function
                .blocks
                .get(predecessor)?
                .successors
                .iter()
                .position(|&successor| successor == natural_loop.header)?;
            let argument = *function
                .blocks
                .get(predecessor)?
                .successor_arguments
                .get(edge)?
                .get(parameter_index)?;
            if natural_loop.blocks.contains(&predecessor) && argument != receiver {
                return None;
            }
            if !natural_loop.blocks.contains(&predecessor) {
                match root {
                    Some(previous) if previous != argument => return None,
                    Some(_) => {}
                    None => root = Some(argument),
                }
            }
        }
        let root = root?;
        return value_is_defined_outside_loop(node_blocks, natural_loop, root).then_some(root);
    }
    value_is_defined_outside_loop(node_blocks, natural_loop, receiver).then_some(receiver)
}

fn value_is_defined_outside_loop(
    node_blocks: &[Option<usize>],
    natural_loop: &NumericNaturalLoop,
    value: NumericValue,
) -> bool {
    node_blocks
        .get(value.0)
        .is_some_and(|block| block.is_some_and(|block| !natural_loop.blocks.contains(&block)))
}

fn same_element_access(
    left: &otter_vm::JitElementAccess,
    right: &otter_vm::JitElementAccess,
) -> bool {
    left.type_tag == right.type_tag
        && left.guards == right.guards
        && left.length_byte == right.length_byte
        && left.length_width == right.length_width
        && left.base == right.base
        && left.element == right.element
}

fn infer_parameter_types(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
    parameter_count: u16,
    register_count: u16,
) -> Option<Vec<NumericType>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut entry = vec![0_u16; width];
    for parameter in 0..parameter_count {
        entry[usize::from(parameter)] = 1_u16.checked_shl(u32::from(parameter))?;
    }
    let mut out_origins = vec![vec![0_u16; width]; blocks.len()];
    let mut int32_parameters = 0_u16;
    let mut number_parameters = 0_u16;

    loop {
        let mut changed = false;
        for (block_index, block) in blocks.iter().enumerate() {
            let mut origins = if block_index == 0 {
                entry.clone()
            } else {
                let mut merged = vec![0_u16; width];
                for &predecessor in &block.predecessors {
                    for (destination, &source) in merged.iter_mut().zip(&out_origins[predecessor]) {
                        *destination |= source;
                    }
                }
                merged
            };
            for pc in block.start..block.end {
                infer_instruction_parameters(
                    view.instructions.get(pc)?,
                    code,
                    &mut origins,
                    &mut int32_parameters,
                    &mut number_parameters,
                )?;
            }
            if origins != out_origins[block_index] {
                out_origins[block_index] = origins;
                changed = true;
            }
        }
        if !changed {
            return Some(
                (0..parameter_count)
                    .map(|parameter| {
                        let bit = 1_u16 << parameter;
                        if int32_parameters & bit != 0 {
                            NumericType::Int32
                        } else if number_parameters & bit != 0 {
                            NumericType::Number
                        } else {
                            NumericType::Tagged
                        }
                    })
                    .collect(),
            );
        }
    }
}

fn infer_instruction_parameters(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    origins: &mut [u16],
    int32_parameters: &mut u16,
    number_parameters: &mut u16,
) -> Option<()> {
    let op = instruction.op(code);
    let (reads, writes) = instruction_accesses(instruction, code)?;
    let read = |index: usize| origins.get(usize::from(*reads.get(index)?)).copied();
    for &register in &reads {
        let _ = origins.get(usize::from(register))?;
    }
    for &register in &writes {
        let _ = origins.get(usize::from(register))?;
    }

    // Every schema-declared write kills parameter provenance unless one of the
    // representation-sensitive transfers below proves an exact value copy.
    // This is deliberately the default: adding a semantic opcode to the
    // bytecode schema must not require another admission mirror here.
    let mut preserved_write = None;
    match op {
        Op::StoreLocal | Op::LoadLocal => {
            preserved_write = Some((*writes.first()?, read(0)?));
        }
        Op::ArrayConstruct => {
            if reads.len() == 1 {
                *int32_parameters |= read(0)?;
            }
        }
        Op::ToPrimitive | Op::ToNumeric | Op::ToNumber => {
            let source = read(0)?;
            *number_parameters |= source;
            preserved_write = Some((*writes.first()?, source));
        }
        Op::Neg | Op::Increment | Op::AddImm | Op::SubImm => {
            let source = read(0)?;
            if !instruction.arith_feedback().is_empty() {
                *number_parameters |= source;
            }
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= source;
                preserved_write = Some((*writes.first()?, source));
            }
        }
        Op::Add
            if instruction
                .arith_feedback()
                .is_primitive_string_concat_only() => {}
        Op::Add | Op::Sub | Op::Mul => {
            let left = read(0)?;
            let right = read(1)?;
            if !instruction.arith_feedback().is_empty() {
                *number_parameters |= left | right;
            }
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= left | right;
                preserved_write = Some((*writes.first()?, left | right));
            }
        }
        Op::Equal | Op::NotEqual | Op::LooseEqual | Op::LooseNotEqual => {
            let inputs = read(0)? | read(1)?;
            if instruction.arith_feedback().is_numeric_only() {
                *number_parameters |= inputs;
                if instruction.arith_feedback().is_int32_only() {
                    *int32_parameters |= inputs;
                }
            }
        }
        Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
            let inputs = read(0)? | read(1)?;
            if !instruction.arith_feedback().is_empty() {
                *number_parameters |= inputs;
            }
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= inputs;
            }
        }
        Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
            let source = read(0)?;
            if !instruction.arith_feedback().is_empty() {
                *number_parameters |= source;
            }
            if instruction.arith_feedback().is_int32_only() {
                *int32_parameters |= source;
            }
        }
        Op::Div
        | Op::Rem
        | Op::Pow
        | Op::BitwiseAnd
        | Op::BitwiseOr
        | Op::BitwiseXor
        | Op::Shl
        | Op::Shr
        | Op::Ushr => {
            let inputs = read(0)? | read(1)?;
            if !instruction.arith_feedback().is_empty() {
                *number_parameters |= inputs;
            }
        }
        Op::BitwiseNot | Op::BitwiseAndImm => {
            let source = read(0)?;
            if !instruction.arith_feedback().is_empty() {
                *number_parameters |= source;
            }
        }
        _ => {}
    }
    for register in writes {
        *origins.get_mut(usize::from(register))? = 0;
    }
    if let Some((register, origin)) = preserved_write {
        *origins.get_mut(usize::from(register))? = origin;
    }
    Some(())
}

fn build_raw_blocks(
    view: &JitCompileSnapshot,
    instruction_semantics: &[InstructionSemantics],
) -> Option<Vec<RawBlock>> {
    let code = view.code_block.as_ref();
    let mut starts = code
        .block_starts()
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    for (pc, instruction) in view.instructions.iter().enumerate() {
        let pc = u32::try_from(pc).ok()?;
        let op = instruction.op(code);
        let binding = opcode_schema(op).binding.is_some();
        let protected_throw = instruction_semantics
            .get(pc as usize)?
            .has_implicit_exception_side_exit(op)
            && code
                .control_flow()
                .enclosing_exception_region(pc)
                .and_then(|region| region.catch_pc)
                .is_some();
        if (binding || protected_throw) && usize::try_from(pc + 1).ok()? < view.instructions.len() {
            starts.insert(pc + 1);
        }
    }
    let starts = starts.into_iter().collect::<Vec<_>>();
    if starts.first().copied() != Some(0) {
        return None;
    }
    let by_pc = starts
        .iter()
        .enumerate()
        .map(|(index, &pc)| (pc, index))
        .collect::<BTreeMap<_, _>>();
    let mut blocks = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = starts
            .get(index + 1)
            .copied()
            .unwrap_or(view.instructions.len() as u32);
        let terminal_pc = end.checked_sub(1)?;
        let instruction = view.instructions.get(terminal_pc as usize)?;
        let op = instruction.op(code);
        let (mut successors, terminator) = match op {
            Op::Jump => (
                vec![target_block(instruction, code, terminal_pc, &by_pc)?],
                RawTerminator::Jump,
            ),
            Op::JumpIfTrue | Op::JumpIfFalse => (
                vec![
                    target_block(instruction, code, terminal_pc, &by_pc)?,
                    *by_pc.get(&end)?,
                ],
                RawTerminator::Branch {
                    when_true: op == Op::JumpIfTrue,
                },
            ),
            Op::Return | Op::ReturnValue => (Vec::new(), RawTerminator::ReturnValue),
            Op::ReturnUndefined => (Vec::new(), RawTerminator::ReturnUndefined),
            _ => (vec![*by_pc.get(&end)?], RawTerminator::Jump),
        };
        let exceptional = instruction_semantics
            .get(terminal_pc as usize)?
            .has_implicit_exception_side_exit(op)
            .then(|| code.control_flow().enclosing_exception_region(terminal_pc))
            .flatten()
            .and_then(|region| Some((*by_pc.get(&region.catch_pc?)?, region.exception_register)));
        let (exceptional_edge, exception_register) = if let Some((handler, register)) = exceptional
        {
            let edge = successors.len();
            successors.push(handler);
            (Some(edge), Some(register))
        } else {
            (None, None)
        };
        blocks.push(RawBlock {
            start: start as usize,
            end: end as usize,
            predecessors: Vec::new(),
            successors,
            exceptional_edge,
            exception_register,
            terminator,
        });
    }
    retain_reachable_raw_blocks(blocks)
}

/// Remove bytecode blocks that can only be entered after leaving generated
/// code through exact deoptimization. In particular, a catch body with no
/// committed pure-exception predecessor resumes in the interpreter after the
/// materialized handler stack is reconstructed; manufacturing a Machine edge
/// would require an exception value that the generated operation never owns.
fn retain_reachable_raw_blocks(blocks: Vec<RawBlock>) -> Option<Vec<RawBlock>> {
    if blocks.is_empty() {
        return None;
    }
    let mut reachable = vec![false; blocks.len()];
    let mut pending = vec![0usize];
    while let Some(block) = pending.pop() {
        let reached = reachable.get_mut(block)?;
        if *reached {
            continue;
        }
        *reached = true;
        pending.extend(blocks.get(block)?.successors.iter().copied());
    }

    let mut remap = vec![None; blocks.len()];
    let mut next = 0usize;
    for (block, &is_reachable) in reachable.iter().enumerate() {
        if is_reachable {
            remap[block] = Some(next);
            next += 1;
        }
    }

    let mut retained = Vec::with_capacity(next);
    for (old_index, mut block) in blocks.into_iter().enumerate() {
        if !reachable[old_index] {
            continue;
        }
        block.predecessors.clear();
        for successor in &mut block.successors {
            *successor = remap.get(*successor).copied().flatten()?;
        }
        retained.push(block);
    }
    for predecessor in 0..retained.len() {
        for successor in retained[predecessor].successors.clone() {
            retained.get_mut(successor)?.predecessors.push(predecessor);
        }
    }
    Some(retained)
}

fn build_liveness(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
    exception_handlers: &[Option<InstructionExceptionHandler>],
    instruction_semantics: &[InstructionSemantics],
    register_count: u16,
) -> Option<Vec<Vec<bool>>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut live_in = vec![vec![false; width]; blocks.len()];
    loop {
        let mut changed = false;
        for block_index in (0..blocks.len()).rev() {
            let block = blocks.get(block_index)?;
            let mut next = block_live_out(block, &live_in, width)?;
            for pc in (block.start..block.end).rev() {
                transfer_instruction_liveness(
                    view.instructions.get(pc)?,
                    code,
                    exception_handlers.get(pc).copied().flatten(),
                    &live_in,
                    *instruction_semantics.get(pc)?,
                    &mut next,
                )?;
            }
            if next != live_in[block_index] {
                live_in[block_index] = next;
                changed = true;
            }
        }
        if !changed {
            return Some(live_in);
        }
    }
}

fn build_instruction_liveness(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
    block_live_in: &[Vec<bool>],
    exception_handlers: &[Option<InstructionExceptionHandler>],
    instruction_semantics: &[InstructionSemantics],
    register_count: u16,
) -> Option<Vec<Vec<bool>>> {
    let code = view.code_block.as_ref();
    let width = usize::from(register_count);
    let mut instruction_live_in = vec![vec![false; width]; view.instructions.len()];
    for block in blocks {
        let mut live = block_live_out(block, block_live_in, width)?;
        for pc in (block.start..block.end).rev() {
            transfer_instruction_liveness(
                view.instructions.get(pc)?,
                code,
                exception_handlers.get(pc).copied().flatten(),
                block_live_in,
                *instruction_semantics.get(pc)?,
                &mut live,
            )?;
            instruction_live_in[pc] = live.clone();
        }
    }
    Some(instruction_live_in)
}

fn build_instruction_exception_handlers(
    view: &JitCompileSnapshot,
    blocks: &[RawBlock],
    instruction_semantics: &[InstructionSemantics],
) -> Option<Vec<Option<InstructionExceptionHandler>>> {
    let code = view.code_block.as_ref();
    let blocks_by_pc = blocks
        .iter()
        .enumerate()
        .map(|(block, raw)| Some((u32::try_from(raw.start).ok()?, block)))
        .collect::<Option<BTreeMap<_, _>>>()?;
    let mut handlers = vec![None; view.instructions.len()];
    for (pc, handler) in handlers.iter_mut().enumerate() {
        let instruction = view.instructions.get(pc)?;
        if !instruction_semantics
            .get(pc)?
            .has_implicit_exception_side_exit(instruction.op(code))
        {
            continue;
        }
        let pc = u32::try_from(pc).ok()?;
        let Some(region) = code.control_flow().enclosing_exception_region(pc) else {
            continue;
        };
        let Some(catch_pc) = region.catch_pc else {
            continue;
        };
        *handler = Some(InstructionExceptionHandler {
            block: *blocks_by_pc.get(&catch_pc)?,
            exception_register: region.exception_register,
        });
    }
    Some(handlers)
}

fn block_live_out(
    block: &RawBlock,
    block_live_in: &[Vec<bool>],
    width: usize,
) -> Option<Vec<bool>> {
    let mut live = vec![false; width];
    for (edge, &successor) in block.successors.iter().enumerate() {
        for (register, &successor_live) in
            block_live_in.get(successor)?.iter().enumerate().take(width)
        {
            if block.exceptional_edge == Some(edge)
                && block.exception_register == u16::try_from(register).ok()
            {
                continue;
            }
            live[register] |= successor_live;
        }
    }
    Some(live)
}

fn transfer_instruction_liveness(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    exception_handler: Option<InstructionExceptionHandler>,
    block_live_in: &[Vec<bool>],
    semantics: InstructionSemantics,
    live: &mut [bool],
) -> Option<()> {
    let (reads, writes) = instruction_accesses(instruction, code)?;
    for write in writes {
        *live.get_mut(usize::from(write))? = false;
    }
    for read in reads {
        *live.get_mut(usize::from(read))? = true;
    }
    if semantics.has_implicit_exception_side_exit(instruction.op(code))
        && let Some(handler) = exception_handler
    {
        for (register, &handler_live) in block_live_in.get(handler.block)?.iter().enumerate() {
            if register != usize::from(handler.exception_register) && handler_live {
                *live.get_mut(register)? = true;
            }
        }
    }
    Some(())
}

fn instruction_accesses(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
) -> Option<(Vec<u16>, Vec<u16>)> {
    let op = instruction.op(code);
    let operands = instruction.operand_view(code);
    let mut reads = Vec::new();
    let mut writes = Vec::new();
    for index in 0..operands.len() {
        let spec = operand_spec_at(op, index)?;
        let operand = operands.get(index)?;
        if OperandKind::of(&operand) != spec.kind {
            return None;
        }
        if spec.register_access == RegisterAccess::None {
            if spec.register_source.is_some() {
                return None;
            }
            continue;
        }
        let register = match (spec.register_source, operand) {
            (Some(RegisterSource::RegisterOperand), Operand::Register(register)) => register,
            (Some(RegisterSource::Imm32RegisterIndex), Operand::Imm32(register)) => {
                u16::try_from(register).ok()?
            }
            _ => return None,
        };
        match spec.register_access {
            RegisterAccess::Read => reads.push(register),
            RegisterAccess::Write => writes.push(register),
            RegisterAccess::None => unreachable!(),
        }
    }
    Some((reads, writes))
}

fn target_block(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    pc: u32,
    by_pc: &BTreeMap<u32, usize>,
) -> Option<usize> {
    let target = i64::from(pc) + 1 + i64::from(instruction.imm32(code, 0)?);
    u32::try_from(target)
        .ok()
        .and_then(|target| by_pc.get(&target).copied())
}

fn merge_predecessors(
    predecessors: &[usize],
    successor: usize,
    blocks: &[RawBlock],
    live_in: &[bool],
    out_states: &[Vec<RegisterState>],
    exceptional_out_states: &[Option<Vec<RegisterState>>],
    nodes: &mut Vec<NumericNode>,
    phi_types: &PhiTypeOverrides,
    requires_mixed_join: &mut bool,
) -> Option<(Vec<RegisterState>, Vec<NumericValue>, Vec<u16>)> {
    let state = |predecessor: usize| {
        let edge = blocks
            .get(predecessor)?
            .successors
            .iter()
            .position(|&target| target == successor)?;
        edge_state(
            predecessor,
            edge,
            blocks,
            out_states,
            exceptional_out_states,
        )
    };
    let first = state(*predecessors.first()?)?.to_vec();
    let mut merged = first.clone();
    let mut parameters = Vec::new();
    let mut parameter_registers = Vec::new();
    for (register, merged_state) in merged.iter_mut().enumerate() {
        if !live_in.get(register).copied().unwrap_or(false) {
            *merged_state = RegisterState::Unset;
            continue;
        }
        let states = predecessors
            .iter()
            .map(|&predecessor| state(predecessor)?.get(register).copied())
            .collect::<Option<Vec<_>>>()?;
        if states.iter().all(|&state| state == states[0]) {
            continue;
        }
        let mut merged_type = None;
        for state in states {
            let RegisterState::Value(value) = state else {
                return None;
            };
            let current = nodes.get(value.0)?.value_type();
            merged_type = Some(match merged_type {
                Some(previous) if previous != current => {
                    *requires_mixed_join = true;
                    join_representation_types(previous, current)?
                }
                Some(previous) => previous,
                None => current,
            });
        }
        let mut merged_type = merged_type?;
        if let Some(requested) = phi_types
            .get(&(successor, u16::try_from(register).ok()?))
            .copied()
        {
            let widened = join_representation_types(merged_type, requested)?;
            if widened != requested {
                return None;
            }
            *requires_mixed_join |= requested != merged_type;
            merged_type = requested;
        }
        let parameter = push(nodes, NumericNode::BlockParameter(merged_type));
        *merged_state = RegisterState::Value(parameter);
        parameters.push(parameter);
        parameter_registers.push(u16::try_from(register).ok()?);
    }
    Some((merged, parameters, parameter_registers))
}

fn edge_state<'a>(
    predecessor: usize,
    edge: usize,
    blocks: &[RawBlock],
    out_states: &'a [Vec<RegisterState>],
    exceptional_out_states: &'a [Option<Vec<RegisterState>>],
) -> Option<&'a [RegisterState]> {
    if blocks.get(predecessor)?.exceptional_edge == Some(edge) {
        exceptional_out_states.get(predecessor)?.as_deref()
    } else {
        out_states.get(predecessor).map(Vec::as_slice)
    }
}

fn force_loop_parameters(
    registers: &mut [RegisterState],
    parameters: &mut Vec<NumericValue>,
    parameter_registers: &mut Vec<u16>,
    nodes: &mut Vec<NumericNode>,
    live_in: &[bool],
    block_index: usize,
    phi_types: &PhiTypeOverrides,
    requires_mixed_join: &mut bool,
) -> Option<()> {
    for (register, state) in registers.iter_mut().enumerate() {
        if !live_in.get(register).copied().unwrap_or(false) {
            *state = RegisterState::Unset;
            continue;
        }
        let RegisterState::Value(value) = *state else {
            continue;
        };
        if parameter_registers.contains(&u16::try_from(register).ok()?) {
            continue;
        }
        let mut value_type = nodes.get(value.0)?.value_type();
        if let Some(requested) = phi_types
            .get(&(block_index, u16::try_from(register).ok()?))
            .copied()
        {
            let widened = join_representation_types(value_type, requested)?;
            if widened != requested {
                return None;
            }
            *requires_mixed_join |= requested != value_type;
            value_type = requested;
        }
        let parameter = push(nodes, NumericNode::BlockParameter(value_type));
        *state = RegisterState::Value(parameter);
        parameters.push(parameter);
        parameter_registers.push(u16::try_from(register).ok()?);
    }
    Some(())
}

fn force_exception_parameters(
    block_index: usize,
    blocks: &[RawBlock],
    registers: &mut [RegisterState],
    parameters: &mut Vec<NumericValue>,
    parameter_registers: &mut Vec<u16>,
    nodes: &mut Vec<NumericNode>,
) -> Option<()> {
    let exception_registers = blocks
        .get(block_index)?
        .predecessors
        .iter()
        .filter_map(|&predecessor| {
            let source = blocks.get(predecessor)?;
            let edge = source
                .successors
                .iter()
                .position(|&successor| successor == block_index)?;
            (source.exceptional_edge == Some(edge))
                .then_some(source.exception_register)
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    for register in exception_registers {
        if parameter_registers.contains(&register) {
            continue;
        }
        let RegisterState::Value(value) = *registers.get(usize::from(register))? else {
            return None;
        };
        if value_type(nodes, value)? != NumericType::Tagged {
            return None;
        }
        let parameter = push(nodes, NumericNode::BlockParameter(NumericType::Tagged));
        *registers.get_mut(usize::from(register))? = RegisterState::Value(parameter);
        parameters.push(parameter);
        parameter_registers.push(register);
    }
    Some(())
}

fn join_representation_types(left: NumericType, right: NumericType) -> Option<NumericType> {
    if left == right {
        return Some(left);
    }
    if matches!(left, NumericType::Tagged | NumericType::Boolean)
        || matches!(right, NumericType::Tagged | NumericType::Boolean)
    {
        return Some(NumericType::Tagged);
    }
    if matches!(
        left,
        NumericType::Int32 | NumericType::Uint32 | NumericType::Number
    ) && matches!(
        right,
        NumericType::Int32 | NumericType::Uint32 | NumericType::Number
    ) {
        return Some(NumericType::Number);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaggedNumericDecode {
    Number,
    Int32,
}

#[derive(Clone, Copy)]
struct NumericDecodeSite<'a> {
    registers: &'a [RegisterState],
    live_in: &'a [bool],
    function_id: u32,
    byte_pc: u32,
}

#[allow(clippy::too_many_arguments)]
fn lower_committed_value(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    operation: CommittedValueOperation,
    semantics: InstructionSemantics,
    registers: &mut [RegisterState],
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    live_in: &[bool],
    frame_states: &mut Vec<NumericFrameState>,
    function_id: u32,
    logical_pc: u32,
    exceptional_edge: Option<usize>,
    exceptional_value: &mut Option<NumericValue>,
) -> Option<()> {
    if !semantics.is_committed_runtime() || semantics.committed_value != Some(operation) {
        return None;
    }
    let (reads, writes) = instruction_accesses(instruction, code)?;
    if reads.len() > 2 || writes.len() > 1 {
        return None;
    }
    let mut inputs = [None, None];
    for (input, register) in inputs.iter_mut().zip(reads) {
        *input = Some(read_value(registers, register)?);
    }
    let value = push(
        nodes,
        NumericNode::CommittedValue {
            operation,
            inputs,
            logical_pc,
            byte_pc: instruction.byte_pc,
            exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
        },
    );
    block_nodes.push(value);
    push_frame_state(
        frame_states,
        NumericFramePoint::Node(value),
        function_id,
        instruction.byte_pc,
        registers,
        live_in,
    );
    record_exceptional_value(exceptional_value, exceptional_edge, value)?;
    if let Some(destination) = writes.first().copied() {
        write(registers, destination, RegisterState::Value(value))?;
    }
    Some(())
}

#[allow(clippy::too_many_arguments)]
fn lower_binding(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    semantics: BindingSemantics,
    registers: &mut [RegisterState],
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    live_in: &[bool],
    frame_states: &mut Vec<NumericFrameState>,
    function_id: u32,
    logical_pc: u32,
    binding_hit_proofs: &rustc_hash::FxHashMap<u32, otter_vm::jit::BindingHitProof>,
    cage_available: bool,
    exceptional_edge: Option<usize>,
    exceptional_value: &mut Option<NumericValue>,
) -> Option<()> {
    let mut inputs = [None, None];
    for (input, operand) in inputs
        .iter_mut()
        .zip(semantics.value_operands().into_iter().flatten())
    {
        let register = register(instruction, code, usize::from(operand))?;
        *input = Some(read_value(registers, register)?);
    }

    let target = match semantics {
        BindingSemantics::Read(BindingRead::GlobalThis { .. }) if cage_available => {
            Some(NumericBindingTarget::GlobalThis)
        }
        BindingSemantics::Read(BindingRead::Upvalue { index, .. })
        | BindingSemantics::Write(BindingWrite::Upvalue { index, .. })
            if cage_available =>
        {
            u32::try_from(instruction.imm32(code, usize::from(index))?)
                .ok()
                .filter(|index| *index <= 4095)
                .map(|index| NumericBindingTarget::Upvalue { index })
        }
        BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Exists { .. })
            if cage_available =>
        {
            binding_hit_proofs
                .get(&instruction.byte_pc)
                .copied()
                .map(NumericBindingTarget::Global)
        }
        BindingSemantics::Write(
            BindingWrite::Global { .. } | BindingWrite::GlobalChecked { .. },
        ) if cage_available => binding_hit_proofs
            .get(&instruction.byte_pc)
            .copied()
            .filter(|proof| match proof {
                otter_vm::jit::BindingHitProof::GlobalLexical { writable, .. }
                | otter_vm::jit::BindingHitProof::GlobalObject { writable, .. } => *writable,
            })
            .map(NumericBindingTarget::Global),
        BindingSemantics::Read(
            BindingRead::Dynamic { .. }
            | BindingRead::ShadowedUpvalue { .. }
            | BindingRead::EvalBindingSeq { .. },
        )
        | BindingSemantics::Write(
            BindingWrite::Dynamic { .. }
            | BindingWrite::ShadowedUpvalue { .. }
            | BindingWrite::ShadowedRestore { .. },
        )
        | BindingSemantics::Delete(_) => None,
        BindingSemantics::Read(BindingRead::GlobalThis { .. } | BindingRead::Upvalue { .. })
        | BindingSemantics::Read(BindingRead::Global { .. } | BindingRead::Exists { .. })
        | BindingSemantics::Write(
            BindingWrite::Global { .. }
            | BindingWrite::GlobalChecked { .. }
            | BindingWrite::Upvalue { .. },
        ) => None,
    };

    let value = push(
        nodes,
        NumericNode::Binding {
            semantics,
            inputs,
            target,
            logical_pc,
            byte_pc: instruction.byte_pc,
            exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
        },
    );
    block_nodes.push(value);
    push_frame_state(
        frame_states,
        NumericFramePoint::Node(value),
        function_id,
        instruction.byte_pc,
        registers,
        live_in,
    );
    record_exceptional_value(exceptional_value, exceptional_edge, value)?;
    if let Some(destination) = semantics.result_operand() {
        write(
            registers,
            register(instruction, code, usize::from(destination))?,
            RegisterState::Value(value),
        )?;
    }
    Some(())
}

fn lower_instruction(
    decline: &mut Option<HirDecline>,
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    derived_constructor: bool,
    registers: &mut [RegisterState],
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    arithmetic_op_count: &mut usize,
    live_in: &[bool],
    frame_states: &mut Vec<NumericFrameState>,
    function_id: u32,
    logical_pc: u32,
    direct_callees: &rustc_hash::FxHashMap<u32, Vec<otter_vm::JitDirectCallee>>,
    direct_constructs: &rustc_hash::FxHashMap<u32, otter_vm::JitDirectCallee>,
    direct_methods: &rustc_hash::FxHashMap<u32, Vec<otter_vm::jit::JitDirectMethod>>,
    constructor_field_transitions: &rustc_hash::FxHashMap<
        u32,
        otter_vm::jit::JitConstructorFieldTransition,
    >,
    element_accesses: &rustc_hash::FxHashMap<u32, otter_vm::JitElementAccess>,
    string_constant_cells: &rustc_hash::FxHashMap<u32, otter_vm::jit::JitStringConstantCell>,
    binding_hit_proofs: &rustc_hash::FxHashMap<u32, otter_vm::jit::BindingHitProof>,
    view: &JitCompileSnapshot,
    direct_call_targets: &mut Vec<NumericDirectCallTarget>,
    operand_values: &mut Vec<NumericValue>,
    semantics: InstructionSemantics,
    exceptional_edge: Option<usize>,
    exceptional_value: &mut Option<NumericValue>,
) -> Option<()> {
    let cage_available = view.cage_base != 0;
    let op = instruction.op(code);
    let local_handler = has_local_exception_handler(code, logical_pc);
    let expects_exceptional_edge = local_handler && semantics.has_implicit_exception_side_exit(op);
    if exceptional_edge.is_some() != expects_exceptional_edge {
        note_decline(
            decline,
            op,
            logical_pc,
            "exceptional edge disagrees with the local handler",
        );
        return None;
    }
    let binding = opcode_schema(op).binding;
    let pure_exception_value = binding.is_some()
        || semantics.committed_value.is_some()
        || matches!(
            op,
            Op::Call
                | Op::CallWithThis
                | Op::CallSpread
                | Op::CallMethodValue
                | Op::New
                | Op::NewSpread
                | Op::SuperConstruct
                | Op::SuperConstructSpread
        );
    if exceptional_edge.is_some() && !pure_exception_value {
        note_decline(
            decline,
            op,
            logical_pc,
            "effectful operation inside a local catch",
        );
        return None;
    }
    if let Some(binding) = binding {
        return lower_binding(
            instruction,
            code,
            binding,
            registers,
            nodes,
            block_nodes,
            live_in,
            frame_states,
            function_id,
            logical_pc,
            binding_hit_proofs,
            cage_available,
            exceptional_edge,
            exceptional_value,
        );
    }
    // A loose comparison with a static nullish operand keeps its direct
    // lowering even when its feedback made the site committed; every other
    // committed site completes through the canonical operation here.
    let nullish_compare = matches!(op, Op::LooseEqual | Op::LooseNotEqual)
        && semantics.committed_value.is_some()
        && {
            let left = read_value(registers, register(instruction, code, 1)?)?;
            let right = read_value(registers, register(instruction, code, 2)?)?;
            value_is_static_nullish(nodes, left) || value_is_static_nullish(nodes, right)
        };
    if let Some(operation) = semantics.committed_value.filter(|_| !nullish_compare) {
        return lower_committed_value(
            instruction,
            code,
            operation,
            semantics,
            registers,
            nodes,
            block_nodes,
            live_in,
            frame_states,
            function_id,
            logical_pc,
            exceptional_edge,
            exceptional_value,
        );
    }
    let decode_site = NumericDecodeSite {
        registers,
        live_in,
        function_id,
        byte_pc: instruction.byte_pc,
    };
    let node = match op {
        Op::Nop | Op::EnterTry | Op::LeaveTry => return Some(()),
        Op::StoreLocal => {
            let value = read_state(registers, register(instruction, code, 0)?)?;
            write(registers, local_index(instruction, code, 1)?, value)?;
            return Some(());
        }
        Op::LoadLocal => {
            let value = read_state(registers, local_index(instruction, code, 1)?)?;
            write(registers, register(instruction, code, 0)?, value)?;
            return Some(());
        }
        Op::LoadUndefined => NumericNode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
        Op::LoadHole => NumericNode::TaggedConstant(otter_vm::Value::hole().to_bits()),
        Op::LoadNull => NumericNode::TaggedConstant(otter_vm::Value::null().to_bits()),
        Op::LoadThis => NumericNode::This,
        Op::LoadString => {
            let _ = instruction.const_index(code, 1)?;
            NumericNode::StringConstantCell {
                byte_pc: instruction.byte_pc,
                target: *string_constant_cells.get(&instruction.byte_pc)?,
            }
        }
        Op::GetPrototype if derived_constructor => {
            let value = push(
                nodes,
                NumericNode::ClassSuperConstructor(read_value(
                    registers,
                    register(instruction, code, 1)?,
                )?),
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::StoreProperty if constructor_field_transitions.contains_key(&instruction.byte_pc) => {
            let _ = instruction.const_index(code, 1)?;
            let value = push(
                nodes,
                NumericNode::ConstructorFieldStore {
                    object: read_value(registers, register(instruction, code, 0)?)?,
                    value: read_value(registers, register(instruction, code, 2)?)?,
                    byte_pc: instruction.byte_pc,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 3)?,
                RegisterState::Unset,
            )?;
            return Some(());
        }
        Op::LoadProperty => {
            let _ = instruction.const_index(code, 2)?;
            // A runtime-backed named-property miss can invoke getters, proxies,
            // and user coercion before throwing. Machine landing pads do not
            // yet reconstruct a committed exceptional state, so keep local
            // handlers on the Template baseline instead of replaying the
            // original operation after an observable effect.
            if exceptional_edge.is_some() {
                note_decline(decline, op, logical_pc, "inside a local catch");
                return None;
            }
            let value = push(
                nodes,
                NumericNode::PropertyLoad {
                    receiver: read_value(registers, register(instruction, code, 1)?)?,
                    byte_pc: instruction.byte_pc,
                    exotic_length: instruction.load_array_length,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::StoreProperty => {
            let _ = instruction.const_index(code, 1)?;
            if exceptional_edge.is_some() {
                note_decline(decline, op, logical_pc, "inside a local catch");
                return None;
            }
            let value = push(
                nodes,
                NumericNode::PropertyStore {
                    receiver: read_value(registers, register(instruction, code, 0)?)?,
                    value: read_value(registers, register(instruction, code, 2)?)?,
                    byte_pc: instruction.byte_pc,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 3)?,
                RegisterState::Unset,
            )?;
            return Some(());
        }
        Op::LoadElement => {
            let receiver = read_value(registers, register(instruction, code, 1)?)?;
            let mut index = read_value(registers, register(instruction, code, 2)?)?;
            let receiver_type = value_type(nodes, receiver)?;
            let index_type = value_type(nodes, index)?;
            let access = element_access_kind(element_accesses, instruction.byte_pc, cage_available);
            let direct = receiver_type == NumericType::Tagged
                && matches!(
                    index_type,
                    NumericType::Tagged
                        | NumericType::Int32
                        | NumericType::Uint32
                        | NumericType::Number
                );
            let Some(access) = access.filter(|_| direct) else {
                // A committed generic miss may throw after performing
                // canonical lookup. Until Machine owns a committed-throw
                // landing contract, keep it out of a local catch.
                if exceptional_edge.is_some() {
                    return None;
                }
                let value = push(
                    nodes,
                    NumericNode::GenericElementLoad {
                        receiver,
                        index,
                        logical_pc,
                        byte_pc: instruction.byte_pc,
                    },
                );
                block_nodes.push(value);
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(value),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
                write(
                    registers,
                    register(instruction, code, 0)?,
                    RegisterState::Value(value),
                )?;
                return Some(());
            };
            if access == NumericElementAccess::Tagged && exceptional_edge.is_some() {
                note_decline(
                    decline,
                    op,
                    logical_pc,
                    "tagged element access inside a local catch",
                );
                return None;
            }
            if access == NumericElementAccess::PackedDouble
                && value_type(nodes, index)? == NumericType::Number
            {
                let checked = push(
                    nodes,
                    NumericNode::CheckedFloat64ToElementIndex {
                        value: index,
                        byte_pc: instruction.byte_pc,
                    },
                );
                block_nodes.push(checked);
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(checked),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
                index = checked;
            }
            let value = push(
                nodes,
                NumericNode::ElementLoad {
                    receiver,
                    index,
                    byte_pc: instruction.byte_pc,
                    access,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::StoreElement => {
            let receiver = read_value(registers, register(instruction, code, 0)?)?;
            let mut index = read_value(registers, register(instruction, code, 1)?)?;
            let source_register = register(instruction, code, 2)?;
            let mut stored = read_value(registers, source_register)?;
            let receiver_type = value_type(nodes, receiver)?;
            let index_type = value_type(nodes, index)?;
            let stored_type = value_type(nodes, stored)?;
            let access = element_access_kind(element_accesses, instruction.byte_pc, cage_available);
            let direct_index = matches!(
                index_type,
                NumericType::Tagged
                    | NumericType::Int32
                    | NumericType::Uint32
                    | NumericType::Number
            );
            let direct_value = match access {
                Some(NumericElementAccess::Tagged) => true,
                Some(NumericElementAccess::PackedDouble) => match stored_type {
                    NumericType::Number | NumericType::Int32 | NumericType::Uint32 => true,
                    NumericType::Tagged => instruction.arith_feedback().is_numeric_only(),
                    NumericType::Boolean => false,
                },
                None => false,
            };
            let Some(access) = access
                .filter(|_| receiver_type == NumericType::Tagged && direct_index && direct_value)
            else {
                // Keep generic stores under the same all-or-nothing exception
                // boundary as loads: canonical reentry cannot exact-deopt into
                // a local handler after the operation has started.
                if exceptional_edge.is_some() {
                    return None;
                }
                let value = push(
                    nodes,
                    NumericNode::GenericElementStore {
                        receiver,
                        index,
                        value: stored,
                        logical_pc,
                        byte_pc: instruction.byte_pc,
                    },
                );
                block_nodes.push(value);
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(value),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
                return Some(());
            };
            if access == NumericElementAccess::Tagged && exceptional_edge.is_some() {
                note_decline(
                    decline,
                    op,
                    logical_pc,
                    "tagged element access inside a local catch",
                );
                return None;
            }
            if access == NumericElementAccess::PackedDouble
                && value_type(nodes, index)? == NumericType::Number
            {
                let checked = push(
                    nodes,
                    NumericNode::CheckedFloat64ToElementIndex {
                        value: index,
                        byte_pc: instruction.byte_pc,
                    },
                );
                block_nodes.push(checked);
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(checked),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
                index = checked;
            }
            if access == NumericElementAccess::PackedDouble {
                stored = match value_type(nodes, stored)? {
                    NumericType::Number => stored,
                    NumericType::Int32 | NumericType::Uint32 => {
                        widen_to_number(stored, nodes, block_nodes)?
                    }
                    NumericType::Tagged if instruction.arith_feedback().is_numeric_only() => {
                        let decoded = read_number(
                            decode_site,
                            nodes,
                            block_nodes,
                            frame_states,
                            source_register,
                            TaggedNumericDecode::Number,
                        )?;
                        widen_to_number(decoded, nodes, block_nodes)?
                    }
                    NumericType::Tagged | NumericType::Boolean => return None,
                };
            }
            let value = push(
                nodes,
                NumericNode::ElementStore {
                    receiver,
                    index,
                    value: stored,
                    byte_pc: instruction.byte_pc,
                    access,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            return Some(());
        }
        Op::LoadInt32 => NumericNode::IntegerConstant(instruction.imm32(code, 1)?),
        Op::LoadTrue => NumericNode::BooleanConstant(true),
        Op::LoadFalse => NumericNode::BooleanConstant(false),
        Op::LoadNumber => {
            instruction.const_index(code, 1)?;
            NumericNode::Constant(instruction.load_number?)
        }
        Op::NewObject | Op::NewArray => {
            let count = if op == Op::NewArray {
                usize::try_from(instruction.const_index(code, 1)?).ok()?
            } else {
                0
            };
            let arguments = (0..count)
                .map(|index| read_value(registers, register(instruction, code, index + 2)?))
                .collect::<Option<Vec<_>>>()?;
            let (argument_start, argument_count) =
                append_operand_values(operand_values, arguments)?;
            let value = push(
                nodes,
                NumericNode::LiteralAllocation {
                    target: if op == Op::NewArray {
                        otter_vm::native_abi::STUB_JIT_NEW_ARRAY
                    } else {
                        otter_vm::native_abi::STUB_JIT_NEW_OBJECT
                    },
                    argument_start,
                    argument_count,
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::ArrayConstruct => {
            let argument_count = usize::try_from(instruction.const_index(code, 1)?).ok()?;
            let length = match argument_count {
                0 => {
                    let length = push(nodes, NumericNode::IntegerConstant(0));
                    block_nodes.push(length);
                    length
                }
                1 => read_int32(
                    decode_site,
                    nodes,
                    block_nodes,
                    frame_states,
                    register(instruction, code, 2)?,
                )?,
                _ => return None,
            };
            let value = push(
                nodes,
                NumericNode::ArrayConstruct {
                    length,
                    byte_pc: instruction.byte_pc,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::Call | Op::CallWithThis => {
            let explicit_receiver = op == Op::CallWithThis;
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let (count_operand, first_argument) = if explicit_receiver { (3, 4) } else { (2, 3) };
            let argument_count =
                usize::try_from(instruction.const_index(code, count_operand)?).ok()?;
            let arguments = (0..argument_count)
                .map(|index| {
                    read_value(
                        registers,
                        register(instruction, code, first_argument + index)?,
                    )
                })
                .collect::<Option<Vec<_>>>()?;
            if !explicit_receiver
                && exceptional_edge.is_none()
                && let Some(target) = view.static_native_calls.get(&instruction.byte_pc).copied()
                && crate::template::arm64::ic_probe::native_leaf_call_is_supported(
                    view,
                    target.leaf_stub_id,
                    argument_count,
                )
                && super::super::native_leaf::descriptor(
                    target,
                    instruction.byte_pc,
                    crate::machine::MachineRepresentation::Tagged,
                )
                .is_some()
            {
                let value_type = if super::super::native_leaf::supports_int32(target.leaf_stub_id)
                    && arguments
                        .iter()
                        .all(|argument| nodes[argument.0].value_type() == NumericType::Int32)
                {
                    NumericType::Int32
                } else {
                    NumericType::Tagged
                };
                let (argument_start, _) = append_operand_values(operand_values, arguments)?;
                let value = push(
                    nodes,
                    NumericNode::NativeLeaf {
                        source,
                        target,
                        value_type,
                        argument_start,
                        byte_pc: instruction.byte_pc,
                    },
                );
                block_nodes.push(value);
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(value),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
                return write(
                    registers,
                    register(instruction, code, 0)?,
                    RegisterState::Value(value),
                );
            }
            let direct = direct_callees
                .get(&instruction.byte_pc)
                .and_then(|targets| targets.first())
                .copied();
            if direct.is_none() && !instruction.call_attempted {
                return lower_cold_call_exit(
                    NumericColdCallKind::Plain,
                    register(instruction, code, 0)?,
                    logical_pc,
                    instruction.byte_pc,
                    exceptional_edge,
                    registers,
                    nodes,
                    block_nodes,
                    frame_states,
                    function_id,
                    live_in,
                    exceptional_value,
                );
            }
            // A plain call with a monomorphic plan keeps its receiver-free
            // linkage. Every other attempted site — an explicit receiver, or a
            // plain call the profile could not settle — carries the receiver
            // as argument word zero: a plain call passes `undefined` there and
            // the callee's own binding rules apply.
            let (target, arguments) = match (direct, explicit_receiver) {
                (Some(callee), false) => (
                    monomorphic_direct_call_target(NumericDirectCallKind::Plain, callee),
                    arguments,
                ),
                (direct, _) => {
                    let receiver = if explicit_receiver {
                        read_value(registers, register(instruction, code, 2)?)?
                    } else {
                        let receiver = push(
                            nodes,
                            NumericNode::TaggedConstant(otter_vm::Value::undefined().to_bits()),
                        );
                        block_nodes.push(receiver);
                        receiver
                    };
                    let target = match direct {
                        Some(callee) => monomorphic_direct_call_target(
                            NumericDirectCallKind::CallWithThis,
                            callee,
                        ),
                        None => generic_call_with_this_target(),
                    };
                    (target, std::iter::once(receiver).chain(arguments).collect())
                }
            };
            let (argument_start, argument_count) =
                append_operand_values(operand_values, arguments)?;
            let target = intern_direct_call_target(direct_call_targets, target)?;
            let value = push(
                nodes,
                NumericNode::DirectCall {
                    source,
                    target,
                    arguments: NumericDirectCallArguments::Fixed {
                        start: argument_start,
                        count: argument_count,
                    },
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                    exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            record_exceptional_value(exceptional_value, exceptional_edge, value)?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::New | Op::SuperConstruct => {
            let direct = direct_constructs.get(&instruction.byte_pc).copied();
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let argument_count = usize::try_from(instruction.const_index(code, 2)?).ok()?;
            let arguments = (0..argument_count)
                .map(|index| read_value(registers, register(instruction, code, 3 + index)?))
                .collect::<Option<Vec<_>>>()?;
            // A base `new` without a generated construct edge — a site the
            // profile could not settle, or whose constructor has no entry
            // generation — completes through the generic construct call once
            // it has been attempted; before that it is an exact cold exit.
            let target = match (direct, op) {
                (Some(callee), _) => {
                    let kind = match (op, callee.plan.is_derived_constructor) {
                        (Op::New, false) => NumericDirectCallKind::Construct,
                        (Op::New, true) => NumericDirectCallKind::DerivedConstruct,
                        (Op::SuperConstruct, false) => NumericDirectCallKind::SuperConstruct,
                        (Op::SuperConstruct, true) => NumericDirectCallKind::DerivedSuperConstruct,
                        _ => return None,
                    };
                    monomorphic_direct_call_target(kind, callee)
                }
                (None, Op::New) if instruction.call_attempted => NumericDirectCallTarget {
                    kind: NumericDirectCallKind::Construct,
                    candidates: Vec::new(),
                },
                (None, Op::New) => {
                    return lower_cold_call_exit(
                        NumericColdCallKind::Plain,
                        register(instruction, code, 0)?,
                        logical_pc,
                        instruction.byte_pc,
                        exceptional_edge,
                        registers,
                        nodes,
                        block_nodes,
                        frame_states,
                        function_id,
                        live_in,
                        exceptional_value,
                    );
                }
                (None, _) => {
                    note_decline(decline, op, logical_pc, "super construct without a plan");
                    return None;
                }
            };
            let (argument_start, argument_count) =
                append_operand_values(operand_values, arguments)?;
            let target = intern_direct_call_target(direct_call_targets, target)?;
            let value = push(
                nodes,
                NumericNode::DirectCall {
                    source,
                    target,
                    arguments: NumericDirectCallArguments::Fixed {
                        start: argument_start,
                        count: argument_count,
                    },
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                    exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            record_exceptional_value(exceptional_value, exceptional_edge, value)?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::CallSpread | Op::NewSpread | Op::SuperConstructSpread => {
            let callee = match op {
                Op::CallSpread => *direct_callees.get(&instruction.byte_pc)?.first()?,
                Op::NewSpread | Op::SuperConstructSpread => {
                    *direct_constructs.get(&instruction.byte_pc)?
                }
                _ => return None,
            };
            let kind = match (op, callee.plan.is_derived_constructor) {
                (Op::CallSpread, _) => NumericDirectCallKind::Plain,
                (Op::NewSpread, false) => NumericDirectCallKind::Construct,
                (Op::NewSpread, true) => NumericDirectCallKind::DerivedConstruct,
                (Op::SuperConstructSpread, false) => NumericDirectCallKind::SuperConstruct,
                (Op::SuperConstructSpread, true) => NumericDirectCallKind::DerivedSuperConstruct,
                _ => return None,
            };
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let arguments = NumericDirectCallArguments::Spread(read_value(
                registers,
                register(instruction, code, 2)?,
            )?);
            let target = intern_direct_call_target(
                direct_call_targets,
                monomorphic_direct_call_target(kind, callee),
            )?;
            let value = push(
                nodes,
                NumericNode::DirectCall {
                    source,
                    target,
                    arguments,
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                    exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            record_exceptional_value(exceptional_value, exceptional_edge, value)?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::CallMethodValue => {
            let _name = instruction.const_index(code, 2)?;
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let argument_count = usize::try_from(instruction.const_index(code, 3)?).ok()?;
            let arguments = (0..argument_count)
                .map(|index| read_value(registers, register(instruction, code, 4 + index)?))
                .collect::<Option<Vec<_>>>()?;
            let target = match direct_methods.get(&instruction.byte_pc) {
                Some(methods) => method_direct_call_target(methods)?,
                None if instruction.call_attempted => generic_method_call_target(),
                None => {
                    return lower_cold_call_exit(
                        NumericColdCallKind::Method,
                        register(instruction, code, 0)?,
                        logical_pc,
                        instruction.byte_pc,
                        exceptional_edge,
                        registers,
                        nodes,
                        block_nodes,
                        frame_states,
                        function_id,
                        live_in,
                        exceptional_value,
                    );
                }
            };
            let (argument_start, argument_count) =
                append_operand_values(operand_values, arguments)?;
            let target = intern_direct_call_target(direct_call_targets, target)?;
            let value = push(
                nodes,
                NumericNode::DirectCall {
                    source,
                    target,
                    arguments: NumericDirectCallArguments::Fixed {
                        start: argument_start,
                        count: argument_count,
                    },
                    logical_pc,
                    byte_pc: instruction.byte_pc,
                    exceptional_edge: exceptional_edge.map(u16::try_from).transpose().ok()?,
                },
            );
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            record_exceptional_value(exceptional_value, exceptional_edge, value)?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::ToPrimitive => {
            instruction.const_index(code, 2)?;
            let value = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                TaggedNumericDecode::Number,
            )?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::ToNumeric | Op::ToNumber => {
            let value = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                TaggedNumericDecode::Number,
            )?;
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::ToBoolean | Op::LogicalNot => {
            let source = read_value(registers, register(instruction, code, 1)?)?;
            let boolean = to_boolean(source, nodes, block_nodes)?;
            if matches!(nodes[boolean.0], NumericNode::TaggedToBoolean(..)) {
                push_frame_state(
                    frame_states,
                    NumericFramePoint::Node(boolean),
                    function_id,
                    instruction.byte_pc,
                    registers,
                    live_in,
                );
            }
            let value = if op == Op::LogicalNot {
                let value = push(nodes, NumericNode::BooleanNot(boolean));
                block_nodes.push(value);
                value
            } else {
                boolean
            };
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::Add
            if instruction
                .arith_feedback()
                .is_primitive_string_concat_only() =>
        {
            let left = read_value(registers, register(instruction, code, 1)?)?;
            let right = read_value(registers, register(instruction, code, 2)?)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            let value = push(nodes, NumericNode::TaggedStringConcat(left, right));
            block_nodes.push(value);
            push_frame_state(
                frame_states,
                NumericFramePoint::Node(value),
                function_id,
                instruction.byte_pc,
                registers,
                live_in,
            );
            write(
                registers,
                register(instruction, code, 0)?,
                RegisterState::Value(value),
            )?;
            return Some(());
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Pow => {
            let feedback = instruction.arith_feedback();
            if !feedback.is_numeric_only() && !feedback.is_empty() {
                note_decline(decline, op, logical_pc, "non-numeric arithmetic feedback");
                return None;
            }
            let tagged_decode =
                if matches!(op, Op::Add | Op::Sub | Op::Mul) && feedback.is_int32_only() {
                    TaggedNumericDecode::Int32
                } else {
                    TaggedNumericDecode::Number
                };
            let left = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            let right = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 2)?,
                tagged_decode,
            )?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            if matches!(op, Op::Add | Op::Sub | Op::Mul)
                && feedback.is_int32_only()
                && value_type(nodes, left)? == NumericType::Int32
                && value_type(nodes, right)? == NumericType::Int32
            {
                match op {
                    Op::Add => NumericNode::IntegerAdd(left, right),
                    Op::Sub => NumericNode::IntegerSub(left, right),
                    Op::Mul => NumericNode::IntegerMul(left, right),
                    _ => unreachable!("matched checked int32 arithmetic"),
                }
            } else {
                let left = widen_to_number(left, nodes, block_nodes)?;
                let right = widen_to_number(right, nodes, block_nodes)?;
                match op {
                    Op::Add => NumericNode::Add(left, right),
                    Op::Sub => NumericNode::Sub(left, right),
                    Op::Mul => NumericNode::Mul(left, right),
                    Op::Div => NumericNode::Div(left, right),
                    Op::Rem => NumericNode::Rem(left, right),
                    Op::Pow => NumericNode::Pow(left, right),
                    _ => unreachable!("matched numeric binary operation"),
                }
            }
        }
        Op::Increment | Op::AddImm | Op::SubImm => {
            let feedback = instruction.arith_feedback();
            if !feedback.is_numeric_only() && !feedback.is_empty() {
                note_decline(decline, op, logical_pc, "non-numeric arithmetic feedback");
                return None;
            }
            let immediate = instruction.imm32(code, 2)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            let tagged_decode = if feedback.is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let source = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            if feedback.is_int32_only() && value_type(nodes, source)? == NumericType::Int32 {
                match op {
                    Op::Increment | Op::AddImm => {
                        NumericNode::IntegerAddImmediate(source, immediate)
                    }
                    Op::SubImm => NumericNode::IntegerSubImmediate(source, immediate),
                    _ => unreachable!("matched immediate int32 operation"),
                }
            } else {
                let source = widen_to_number(source, nodes, block_nodes)?;
                let immediate = push(nodes, NumericNode::Constant(f64::from(immediate)));
                block_nodes.push(immediate);
                match op {
                    Op::Increment | Op::AddImm => NumericNode::Add(source, immediate),
                    Op::SubImm => NumericNode::Sub(source, immediate),
                    _ => unreachable!("matched guarded immediate operation"),
                }
            }
        }
        Op::BitwiseAndImm => {
            let source = read_int32_bits(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
            )?;
            let immediate = instruction.imm32(code, 2)?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            NumericNode::IntegerAndImmediate(source, immediate)
        }
        Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
            let feedback = instruction.arith_feedback();
            if !feedback.is_numeric_only() && !feedback.is_empty() {
                note_decline(decline, op, logical_pc, "non-numeric arithmetic feedback");
                return None;
            }
            let immediate = instruction.imm32(code, 2)?;
            let tagged_decode = if feedback.is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let source = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            if feedback.is_int32_only() && value_type(nodes, source)? == NumericType::Int32 {
                match op {
                    Op::LessThanImm => NumericNode::IntegerLessThanImmediate(source, immediate),
                    Op::EqualImm => NumericNode::IntegerEqualImmediate(source, immediate),
                    Op::NotEqualImm => NumericNode::IntegerNotEqualImmediate(source, immediate),
                    _ => unreachable!("matched immediate int32 comparison"),
                }
            } else {
                let source = widen_to_number(source, nodes, block_nodes)?;
                let immediate = push(nodes, NumericNode::Constant(f64::from(immediate)));
                block_nodes.push(immediate);
                match op {
                    Op::LessThanImm => NumericNode::LessThan(source, immediate),
                    Op::EqualImm => NumericNode::Equal(source, immediate),
                    Op::NotEqualImm => NumericNode::NotEqual(source, immediate),
                    _ => unreachable!("matched guarded immediate comparison"),
                }
            }
        }
        Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor | Op::Shl | Op::Shr | Op::Ushr => {
            let left = read_int32_bits(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
            )?;
            let right = read_int32_bits(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 2)?,
            )?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            match op {
                Op::BitwiseAnd => NumericNode::IntegerAnd(left, right),
                Op::BitwiseOr => NumericNode::IntegerOr(left, right),
                Op::BitwiseXor => NumericNode::IntegerXor(left, right),
                Op::Shl => NumericNode::IntegerShiftLeft(left, right),
                Op::Shr => NumericNode::IntegerShiftRight(left, right),
                Op::Ushr => NumericNode::IntegerShiftRightLogical(left, right),
                _ => unreachable!("matched binary int32 operation"),
            }
        }
        Op::BitwiseNot => {
            let source = read_int32_bits(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
            )?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            NumericNode::IntegerNot(source)
        }
        Op::Neg => {
            let feedback = instruction.arith_feedback();
            if !feedback.is_numeric_only() && !feedback.is_empty() {
                note_decline(decline, op, logical_pc, "non-numeric arithmetic feedback");
                return None;
            }
            let tagged_decode = if feedback.is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let source = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            *arithmetic_op_count = arithmetic_op_count.checked_add(1)?;
            if feedback.is_int32_only() && value_type(nodes, source)? == NumericType::Int32 {
                NumericNode::IntegerNeg(source)
            } else {
                NumericNode::Neg(widen_to_number(source, nodes, block_nodes)?)
            }
        }
        Op::Equal | Op::NotEqual | Op::LooseEqual | Op::LooseNotEqual => {
            let feedback = instruction.arith_feedback();
            let speculate_unseen_loose_numeric =
                feedback.is_empty() && matches!(op, Op::LooseEqual | Op::LooseNotEqual) && {
                    let left = read_value(registers, register(instruction, code, 1)?)?;
                    let right = read_value(registers, register(instruction, code, 2)?)?;
                    !value_is_static_nullish(nodes, left) && !value_is_static_nullish(nodes, right)
                };
            if !feedback.is_numeric_only() && !speculate_unseen_loose_numeric {
                let left = read_value(registers, register(instruction, code, 1)?)?;
                let right = read_value(registers, register(instruction, code, 2)?)?;
                let value = if matches!(op, Op::Equal | Op::NotEqual) {
                    let equal = push(nodes, NumericNode::TaggedStrictEqual(left, right));
                    block_nodes.push(equal);
                    push_frame_state(
                        frame_states,
                        NumericFramePoint::Node(equal),
                        function_id,
                        instruction.byte_pc,
                        registers,
                        live_in,
                    );
                    if op == Op::NotEqual {
                        let value = push(nodes, NumericNode::BooleanNot(equal));
                        block_nodes.push(value);
                        value
                    } else {
                        equal
                    }
                } else {
                    let left_nullish = value_is_static_nullish(nodes, left);
                    let right_nullish = value_is_static_nullish(nodes, right);
                    let source = match (left_nullish, right_nullish) {
                        (true, true) => {
                            let value =
                                push(nodes, NumericNode::BooleanConstant(op == Op::LooseEqual));
                            block_nodes.push(value);
                            write(
                                registers,
                                register(instruction, code, 0)?,
                                RegisterState::Value(value),
                            )?;
                            return Some(());
                        }
                        (true, false) => right,
                        (false, true) => left,
                        (false, false) => {
                            note_decline(
                                decline,
                                op,
                                logical_pc,
                                "coercive loose equality without committed feedback",
                            );
                            return None;
                        }
                    };
                    if value_type(nodes, source)? != NumericType::Tagged {
                        note_decline(decline, op, logical_pc, "nullish operand is not tagged");
                        return None;
                    }
                    let value = push(
                        nodes,
                        NumericNode::TaggedNullishEqual {
                            value: source,
                            equal: op == Op::LooseEqual,
                            byte_pc: instruction.byte_pc,
                        },
                    );
                    block_nodes.push(value);
                    push_frame_state(
                        frame_states,
                        NumericFramePoint::Node(value),
                        function_id,
                        instruction.byte_pc,
                        registers,
                        live_in,
                    );
                    value
                };
                write(
                    registers,
                    register(instruction, code, 0)?,
                    RegisterState::Value(value),
                )?;
                return Some(());
            }
            let tagged_decode = if feedback.is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let left = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            let right = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 2)?,
                tagged_decode,
            )?;
            if feedback.is_int32_only()
                && value_type(nodes, left)? == NumericType::Int32
                && value_type(nodes, right)? == NumericType::Int32
            {
                match op {
                    Op::Equal | Op::LooseEqual => NumericNode::IntegerEqual(left, right),
                    Op::NotEqual | Op::LooseNotEqual => NumericNode::IntegerNotEqual(left, right),
                    _ => unreachable!("matched equality"),
                }
            } else {
                let left = widen_to_number(left, nodes, block_nodes)?;
                let right = widen_to_number(right, nodes, block_nodes)?;
                match op {
                    Op::Equal | Op::LooseEqual => NumericNode::Equal(left, right),
                    Op::NotEqual | Op::LooseNotEqual => NumericNode::NotEqual(left, right),
                    _ => unreachable!("matched equality"),
                }
            }
        }
        Op::LessThan | Op::LessEq | Op::GreaterThan | Op::GreaterEq => {
            let feedback = instruction.arith_feedback();
            if !feedback.is_numeric_only() && !feedback.is_empty() {
                note_decline(decline, op, logical_pc, "non-numeric arithmetic feedback");
                return None;
            }
            let tagged_decode = if feedback.is_int32_only() {
                TaggedNumericDecode::Int32
            } else {
                TaggedNumericDecode::Number
            };
            let left = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 1)?,
                tagged_decode,
            )?;
            let right = read_number(
                decode_site,
                nodes,
                block_nodes,
                frame_states,
                register(instruction, code, 2)?,
                tagged_decode,
            )?;
            if feedback.is_int32_only()
                && value_type(nodes, left)? == NumericType::Int32
                && value_type(nodes, right)? == NumericType::Int32
            {
                match op {
                    Op::LessThan => NumericNode::IntegerLessThan(left, right),
                    Op::LessEq => NumericNode::IntegerLessEqual(left, right),
                    Op::GreaterThan => NumericNode::IntegerGreaterThan(left, right),
                    Op::GreaterEq => NumericNode::IntegerGreaterEqual(left, right),
                    _ => unreachable!("matched int32 comparison"),
                }
            } else {
                let left = widen_to_number(left, nodes, block_nodes)?;
                let right = widen_to_number(right, nodes, block_nodes)?;
                match op {
                    Op::LessThan => NumericNode::LessThan(left, right),
                    Op::LessEq => NumericNode::LessEqual(left, right),
                    Op::GreaterThan => NumericNode::GreaterThan(left, right),
                    Op::GreaterEq => NumericNode::GreaterEqual(left, right),
                    _ => unreachable!("matched Float64 comparison"),
                }
            }
        }
        _ => {
            note_decline(decline, op, logical_pc, "opcode outside the Machine HIR");
            return None;
        }
    };
    let destination = register(instruction, code, 0)?;
    let value = push(nodes, node);
    block_nodes.push(value);
    if matches!(
        node,
        NumericNode::IntegerAdd(..)
            | NumericNode::IntegerSub(..)
            | NumericNode::IntegerMul(..)
            | NumericNode::IntegerNeg(..)
            | NumericNode::IntegerAddImmediate(..)
            | NumericNode::IntegerSubImmediate(..)
    ) {
        push_frame_state(
            frame_states,
            NumericFramePoint::Node(value),
            function_id,
            instruction.byte_pc,
            registers,
            live_in,
        );
    }
    write(registers, destination, RegisterState::Value(value))
}

fn push_frame_state(
    frame_states: &mut Vec<NumericFrameState>,
    point: NumericFramePoint,
    function_id: u32,
    byte_pc: u32,
    registers: &[RegisterState],
    live_in: &[bool],
) {
    frame_states.push(NumericFrameState {
        point,
        function_id,
        byte_pc,
        slots: registers
            .iter()
            .copied()
            .zip(live_in.iter().copied())
            .map(|(state, live)| match (state, live) {
                (RegisterState::Value(value), true) => NumericFrameSlot::Value(value),
                (RegisterState::Unset, _) | (RegisterState::Value(_), false) => {
                    NumericFrameSlot::Undefined
                }
            })
            .collect(),
    });
}

fn has_local_exception_handler(code: &otter_vm::CodeBlock, logical_pc: u32) -> bool {
    code.control_flow()
        .enclosing_exception_region(logical_pc)
        .is_some_and(|region| region.catch_pc.is_some())
}

fn element_access_kind(
    element_accesses: &rustc_hash::FxHashMap<u32, otter_vm::JitElementAccess>,
    byte_pc: u32,
    cage_available: bool,
) -> Option<NumericElementAccess> {
    let access = cage_available
        .then(|| element_accesses.get(&byte_pc))
        .flatten()?;
    if access.type_tag == 0 || matches!(access.base, JitElementBase::None) {
        return None;
    }
    if packed_double_element_access_is_exact(access) {
        Some(NumericElementAccess::PackedDouble)
    } else if access.element == JitElementRepr::Float64
        && matches!(access.base, JitElementBase::InBody { .. })
    {
        // An in-body Float64 declaration without both ordinary-Array guards
        // could reinterpret tagged words after a storage-kind transition.
        None
    } else {
        Some(NumericElementAccess::Tagged)
    }
}

fn push(nodes: &mut Vec<NumericNode>, node: NumericNode) -> NumericValue {
    let value = NumericValue(nodes.len());
    nodes.push(node);
    value
}

fn register(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    index: usize,
) -> Option<u16> {
    match instruction.operand(code, index) {
        Some(Operand::Register(register)) => Some(register),
        _ => None,
    }
}

fn local_index(
    instruction: &JitInstructionMetadata,
    code: &otter_vm::CodeBlock,
    index: usize,
) -> Option<u16> {
    u16::try_from(instruction.imm32(code, index)?).ok()
}

fn read_state(registers: &[RegisterState], register: u16) -> Option<RegisterState> {
    match registers.get(usize::from(register)).copied()? {
        RegisterState::Unset => None,
        RegisterState::Value(value) => Some(RegisterState::Value(value)),
    }
}

fn read_number(
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    register: u16,
    tagged_decode: TaggedNumericDecode,
) -> Option<NumericValue> {
    let value = read_value(site.registers, register)?;
    match value_type(nodes, value)? {
        NumericType::Int32 | NumericType::Uint32 | NumericType::Number => Some(value),
        NumericType::Tagged => Some(push_tagged_numeric_decode(
            site,
            nodes,
            block_nodes,
            frame_states,
            value,
            tagged_decode,
        )),
        NumericType::Boolean => None,
    }
}

fn read_value(registers: &[RegisterState], register: u16) -> Option<NumericValue> {
    let RegisterState::Value(value) = read_state(registers, register)? else {
        return None;
    };
    Some(value)
}

fn read_int32(
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    register: u16,
) -> Option<NumericValue> {
    let value = read_value(site.registers, register)?;
    match value_type(nodes, value)? {
        NumericType::Int32 => Some(value),
        NumericType::Tagged => Some(push_tagged_numeric_decode(
            site,
            nodes,
            block_nodes,
            frame_states,
            value,
            TaggedNumericDecode::Int32,
        )),
        NumericType::Uint32 | NumericType::Number | NumericType::Boolean => None,
    }
}

fn read_int32_bits(
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    register: u16,
) -> Option<NumericValue> {
    let value = read_value(site.registers, register)?;
    let node = match value_type(nodes, value)? {
        NumericType::Int32 | NumericType::Uint32 => return Some(value),
        NumericType::Number => NumericNode::FloatToInt32(value),
        NumericType::Boolean => NumericNode::BooleanToInt32(value),
        NumericType::Tagged => {
            let number = push_tagged_numeric_decode(
                site,
                nodes,
                block_nodes,
                frame_states,
                value,
                TaggedNumericDecode::Number,
            );
            NumericNode::FloatToInt32(number)
        }
    };
    let coerced = push(nodes, node);
    block_nodes.push(coerced);
    Some(coerced)
}

fn push_tagged_numeric_decode(
    site: NumericDecodeSite<'_>,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
    frame_states: &mut Vec<NumericFrameState>,
    source: NumericValue,
    decode: TaggedNumericDecode,
) -> NumericValue {
    let node = match decode {
        TaggedNumericDecode::Number => NumericNode::TaggedToNumber(source),
        TaggedNumericDecode::Int32 => NumericNode::TaggedToInt32(source),
    };
    let value = push(nodes, node);
    block_nodes.push(value);
    push_frame_state(
        frame_states,
        NumericFramePoint::Node(value),
        site.function_id,
        site.byte_pc,
        site.registers,
        site.live_in,
    );
    value
}

fn value_type(nodes: &[NumericNode], value: NumericValue) -> Option<NumericType> {
    nodes.get(value.0).copied().map(NumericNode::value_type)
}

fn value_is_static_nullish(nodes: &[NumericNode], value: NumericValue) -> bool {
    let Some(NumericNode::TaggedConstant(bits)) = nodes.get(value.0) else {
        return false;
    };
    *bits == otter_vm::Value::null().to_bits() || *bits == otter_vm::Value::undefined().to_bits()
}

fn to_boolean(
    value: NumericValue,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
) -> Option<NumericValue> {
    let node = match value_type(nodes, value)? {
        NumericType::Boolean => return Some(value),
        NumericType::Int32 | NumericType::Uint32 => NumericNode::IntegerToBoolean(value),
        NumericType::Number => NumericNode::FloatToBoolean(value),
        NumericType::Tagged => NumericNode::TaggedToBoolean(value),
    };
    let boolean = push(nodes, node);
    block_nodes.push(boolean);
    Some(boolean)
}

fn widen_to_number(
    value: NumericValue,
    nodes: &mut Vec<NumericNode>,
    block_nodes: &mut Vec<NumericValue>,
) -> Option<NumericValue> {
    match value_type(nodes, value)? {
        NumericType::Number => Some(value),
        NumericType::Int32 => {
            let widened = push(nodes, NumericNode::WidenInt32(value));
            block_nodes.push(widened);
            Some(widened)
        }
        NumericType::Uint32 => {
            let widened = push(nodes, NumericNode::WidenUint32(value));
            block_nodes.push(widened);
            Some(widened)
        }
        NumericType::Boolean => None,
        NumericType::Tagged => None,
    }
}

fn write(registers: &mut [RegisterState], register: u16, value: RegisterState) -> Option<()> {
    *registers.get_mut(usize::from(register))? = value;
    Some(())
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{NO_HANDLER_OFFSET, Op, Operand};
    use otter_vm::{
        JitCompileSnapshot, JitDirectCallThisMode, JitDirectCallee, JitElementAccess,
        jit::{
            BindingHitProof, JitConstructorFieldTransition, JitDirectCallPlan, JitDirectMethod,
            JitMethodGuard, JitStringConstantCell, JitTestInstruction,
        },
        jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
        native_abi::NativeFrameKind,
    };

    use super::*;

    fn direct_callee(function_id: u32) -> JitDirectCallee {
        JitDirectCallee {
            plan: JitDirectCallPlan {
                function_id,
                code_object_id: u64::from(function_id) + 1,
                entry_cell: u64::from(function_id) + 2,
                tier: NativeFrameKind::Baseline,
                this_mode: JitDirectCallThisMode::StrictOrLexical,
                is_derived_constructor: false,
                generated_stack_frame_bytes: Some(0),
                param_count: 1,
                register_count: 2,
                own_upvalue_count: 0,
                inherited_upvalue_count: 0,
                needs_incoming_arguments: false,
            },
            receiver_allocation: None,
        }
    }

    fn direct_method(target_index: u32, target_count: u32, function_id: u32) -> JitDirectMethod {
        JitDirectMethod {
            target_index,
            target_count,
            guard: JitMethodGuard {
                method_fid: function_id,
                recv_shape: 10 + target_index,
                proto_chain: vec![20 + target_index],
                method_value_byte: 32 + target_index * 8,
            },
            callee: direct_callee(function_id),
        }
    }

    fn call_view_with_arguments(method: bool, argument_registers: &[u16]) -> JitCompileSnapshot {
        let destination = argument_registers
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .expect("test destination register");
        let argument_count = u32::try_from(argument_registers.len()).expect("test argument count");
        let call = if method {
            let mut operands = vec![
                Operand::Register(destination),
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::ConstIndex(argument_count),
            ];
            operands.extend(argument_registers.iter().copied().map(Operand::Register));
            JitTestInstruction::new(Op::CallMethodValue, 0, 0, operands)
        } else {
            let mut operands = vec![
                Operand::Register(destination),
                Operand::Register(0),
                Operand::ConstIndex(argument_count),
            ];
            operands.extend(argument_registers.iter().copied().map(Operand::Register));
            JitTestInstruction::new(Op::Call, 0, 0, operands)
        };
        JitCompileSnapshot::without_feedback(
            110,
            destination,
            destination.checked_add(1).expect("test register count"),
            vec![
                call,
                JitTestInstruction::new(
                    Op::ReturnValue,
                    1,
                    8,
                    vec![Operand::Register(destination)],
                ),
            ],
        )
    }

    fn call_view(method: bool) -> JitCompileSnapshot {
        call_view_with_arguments(method, &[1])
    }

    #[test]
    fn native_leaf_selection_requires_exact_arity_and_preserves_pre_call_state() {
        use crate::machine::{CallEffects, CallTarget, MachineOpcode, native_leaf};
        use otter_vm::native_abi::{STUB_MATH_MAX_LEAF, STUB_PARSE_INT_I32_LEAF};
        for (stub, argument_count) in [(STUB_PARSE_INT_I32_LEAF, 1), (STUB_MATH_MAX_LEAF, 2)] {
            for count in 0..=3 {
                let arguments = vec![1; count];
                let mut view = call_view_with_arguments(false, &arguments);
                view.native_ref_byte = 8;
                view.seed_call_attempted_for_test(0);
                view.static_native_calls.insert(
                    0,
                    otter_vm::JitStaticNativeCall {
                        builtin_native_ref: 7,
                        leaf_stub_id: stub.id,
                        argument_count,
                    },
                );
                let hir = NumericFunction::build(&view).expect("static-native HIR");
                let leaf = hir
                    .nodes
                    .iter()
                    .position(|node| matches!(node, NumericNode::NativeLeaf { .. }));
                if count != usize::from(argument_count) {
                    assert!(
                        leaf.is_none(),
                        "arity mismatch must retain the canonical call"
                    );
                    assert!(
                        hir.nodes
                            .iter()
                            .any(|node| matches!(node, NumericNode::DirectCall { .. }))
                    );
                    continue;
                }
                let leaf = NumericValue(leaf.expect("exact-arity native leaf"));
                let state = hir
                    .frame_states
                    .iter()
                    .find(|state| state.point == NumericFramePoint::Node(leaf))
                    .expect("pre-call state");
                assert_eq!(state.byte_pc, 0);
                assert!(
                    !state.slots.contains(&NumericFrameSlot::Value(leaf)),
                    "result is not defined on a miss"
                );
                let sequence = super::super::select_with_packed_double_view_caches(
                    &hir,
                    &NumericPackedDoubleViewCachePlan::default(),
                )
                .expect("verified native leaf Machine IR");
                sequence.verify().expect("complete ABI verification");
                let (instruction, descriptor) = sequence
                    .instructions()
                    .iter()
                    .find_map(|instruction| {
                        let MachineOpcode::Call(index) = instruction.opcode else {
                            return None;
                        };
                        let descriptor = &sequence.call_descriptors()[index as usize];
                        matches!(descriptor.target, CallTarget::NativeLeaf { .. })
                            .then_some((instruction, descriptor))
                    })
                    .expect("selected leaf descriptor");
                assert!(native_leaf::is_valid(descriptor, instruction));
                let mut invalid = descriptor.clone();
                invalid.effects = CallEffects::REENTRANT;
                assert!(!native_leaf::is_valid(&invalid, instruction));
                let mut invalid = instruction.clone();
                invalid.deopt = None;
                assert!(!native_leaf::is_valid(descriptor, &invalid));
                invalid = instruction.clone();
                invalid.operands.swap(0, 1);
                assert!(
                    !native_leaf::is_valid(descriptor, &invalid),
                    "callee and argument ABI words cannot alias"
                );
            }
        }
    }

    fn array_construct_view(argument_count: u32) -> JitCompileSnapshot {
        let mut operands = vec![Operand::Register(2), Operand::ConstIndex(argument_count)];
        operands
            .extend((0..argument_count).map(|register| {
                Operand::Register(u16::try_from(register).expect("test register"))
            }));
        JitCompileSnapshot::without_feedback(
            111,
            2,
            3,
            vec![
                JitTestInstruction::new(Op::ArrayConstruct, 0, 0, operands),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(2)]),
            ],
        )
    }

    fn tagged_array_construct_view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            112,
            1,
            3,
            vec![
                JitTestInstruction::new(
                    Op::LoadProperty,
                    0,
                    0,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                JitTestInstruction::new(
                    Op::ArrayConstruct,
                    1,
                    8,
                    vec![
                        Operand::Register(2),
                        Operand::ConstIndex(1),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 2, 16, vec![Operand::Register(2)]),
            ],
        )
    }

    fn number_index_element_view() -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            113,
            3,
            5,
            vec![
                JitTestInstruction::new(
                    Op::Mul,
                    0,
                    0,
                    vec![
                        Operand::Register(3),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(
                    Op::LoadElement,
                    1,
                    8,
                    vec![
                        Operand::Register(4),
                        Operand::Register(0),
                        Operand::Register(3),
                    ],
                ),
                JitTestInstruction::new(
                    Op::StoreElement,
                    2,
                    16,
                    vec![
                        Operand::Register(0),
                        Operand::Register(3),
                        Operand::Register(4),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(4)]),
            ],
        );
        view.cage_base = 0x1000;
        for byte_pc in [8, 16] {
            view.element_accesses.insert(
                byte_pc,
                JitElementAccess {
                    type_tag: 1,
                    base: JitElementBase::InBody { byte: 8 },
                    ..JitElementAccess::default()
                },
            );
        }
        view
    }

    fn packed_double_number_index_element_view() -> JitCompileSnapshot {
        let mut view = number_index_element_view();
        for access in view.element_accesses.values_mut() {
            *access = JitElementAccess::packed_double_array();
        }
        view
    }

    fn packed_double_loop_view() -> JitCompileSnapshot {
        let instructions = vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(2)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(4), Operand::Register(3)],
            ),
            (
                Op::LoadElement,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(3),
                ],
            ),
            (
                Op::AddImm,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Imm32(1),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-6)]),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ];
        let mut view = JitCompileSnapshot::without_feedback(
            151,
            1,
            4,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
        view.seed_arith_feedback_for_test(6, ArithFeedback::from_bits(ARITH_INT32));
        view.cage_base = 0x1000;
        for byte_pc in [32, 40] {
            view.element_accesses
                .insert(byte_pc, JitElementAccess::packed_double_array());
        }
        view
    }

    fn packed_double_cache_function(
        varying_receiver: bool,
        unsafe_loop: bool,
    ) -> (
        NumericFunction,
        JitCompileSnapshot,
        NumericValue,
        NumericValue,
    ) {
        let value = NumericValue;
        let mut nodes = vec![
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Tagged,
            },
            NumericNode::BlockParameter(NumericType::Tagged),
            NumericNode::IntegerConstant(0),
            NumericNode::ElementLoad {
                receiver: value(1),
                index: value(2),
                byte_pc: 24,
                access: NumericElementAccess::PackedDouble,
            },
            NumericNode::ElementStore {
                receiver: value(1),
                index: value(2),
                value: value(3),
                byte_pc: 32,
                access: NumericElementAccess::PackedDouble,
            },
            NumericNode::BooleanConstant(true),
        ];
        let mut loop_nodes = vec![value(2), value(3), value(4)];
        if unsafe_loop {
            nodes.push(NumericNode::ArrayConstruct {
                length: value(2),
                byte_pc: 40,
            });
            loop_nodes.push(value(6));
        }
        let backedge_receiver = if varying_receiver { value(2) } else { value(1) };
        let function = NumericFunction {
            function_id: 150,
            nodes,
            blocks: vec![
                NumericBlock {
                    logical_pc: 0,
                    osr_entry_allowed: true,
                    predecessors: Vec::new(),
                    successors: vec![1],
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: vec![vec![value(0)]],
                    nodes: vec![value(0)],
                    terminator: NumericTerminator::Jump,
                },
                NumericBlock {
                    logical_pc: 1,
                    osr_entry_allowed: true,
                    predecessors: vec![0, 2],
                    successors: vec![2, 3],
                    parameters: vec![value(1)],
                    parameter_registers: vec![0],
                    successor_arguments: vec![Vec::new(), Vec::new()],
                    nodes: vec![value(1), value(5)],
                    terminator: NumericTerminator::Branch {
                        condition: value(5),
                        when_true: true,
                    },
                },
                NumericBlock {
                    logical_pc: 2,
                    osr_entry_allowed: true,
                    predecessors: vec![1],
                    successors: vec![1],
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: vec![vec![backedge_receiver]],
                    nodes: loop_nodes,
                    terminator: NumericTerminator::Jump,
                },
                NumericBlock {
                    logical_pc: 3,
                    osr_entry_allowed: true,
                    predecessors: vec![1],
                    successors: Vec::new(),
                    parameters: Vec::new(),
                    parameter_registers: Vec::new(),
                    successor_arguments: Vec::new(),
                    nodes: Vec::new(),
                    terminator: NumericTerminator::Return(value(0)),
                },
            ],
            frame_states: Vec::new(),
            direct_call_targets: Vec::new(),
            operand_values: Vec::new(),
            parameter_count: 1,
            register_count: 6,
            arithmetic_op_count: 0,
        };
        let mut view = JitCompileSnapshot::without_feedback(
            150,
            1,
            1,
            vec![JitTestInstruction::new(
                Op::ReturnValue,
                0,
                0,
                vec![Operand::Register(0)],
            )],
        );
        view.element_accesses
            .insert(24, JitElementAccess::packed_double_array());
        view.element_accesses
            .insert(32, JitElementAccess::packed_double_array());
        (function, view, value(3), value(4))
    }

    fn global_load_view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            113,
            1,
            2,
            vec![
                JitTestInstruction::new(
                    Op::LoadGlobalOrThrow,
                    0,
                    24,
                    vec![Operand::Register(1), Operand::ConstIndex(0)],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(0)]),
            ],
        )
    }

    fn loose_nullish_view(op: Op, literal: Op, literal_left: bool) -> JitCompileSnapshot {
        debug_assert!(matches!(op, Op::LooseEqual | Op::LooseNotEqual));
        debug_assert!(matches!(literal, Op::LoadNull | Op::LoadUndefined));
        let (left, right) = if literal_left {
            (Operand::Register(1), Operand::Register(0))
        } else {
            (Operand::Register(0), Operand::Register(1))
        };
        JitCompileSnapshot::without_feedback(
            114,
            1,
            3,
            vec![
                JitTestInstruction::new(literal, 0, 0, vec![Operand::Register(1)]),
                JitTestInstruction::new(op, 1, 8, vec![Operand::Register(2), left, right]),
                JitTestInstruction::new(Op::ReturnValue, 2, 16, vec![Operand::Register(2)]),
            ],
        )
    }

    fn loose_numeric_view(op: Op, feedback: ArithFeedback) -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            115,
            2,
            3,
            vec![
                JitTestInstruction::new(
                    op,
                    0,
                    24,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(2)]),
            ],
        );
        view.seed_arith_feedback_for_test(0, feedback);
        view
    }

    fn unseen_immediate_numeric_view(op: Op) -> JitCompileSnapshot {
        debug_assert!(matches!(
            op,
            Op::LessThanImm | Op::EqualImm | Op::NotEqualImm
        ));
        JitCompileSnapshot::without_feedback(
            119,
            1,
            2,
            vec![
                JitTestInstruction::new(
                    op,
                    0,
                    24,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::Imm32(7),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(1)]),
            ],
        )
    }

    fn unseen_div_view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            120,
            2,
            3,
            vec![
                JitTestInstruction::new(
                    Op::Div,
                    0,
                    40,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 48, vec![Operand::Register(2)]),
            ],
        )
    }

    fn mixed_numeric_loop_view() -> JitCompileSnapshot {
        let instructions = vec![
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
            (
                Op::LessThanImm,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::Imm32(3),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(4), Operand::Register(1)],
            ),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(2)]),
            (
                Op::Div,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(2),
                ],
            ),
            (
                Op::StoreLocal,
                vec![Operand::Register(3), Operand::Imm32(0)],
            ),
            (Op::Jump, vec![Operand::Imm32(-6)]),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ];
        JitCompileSnapshot::without_feedback(
            121,
            0,
            4,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        )
    }

    fn catch_liveness_view_with_element_store(store: bool) -> JitCompileSnapshot {
        let element = if store {
            (
                Op::StoreElement,
                vec![
                    Operand::Register(1),
                    Operand::Register(2),
                    Operand::Register(5),
                ],
            )
        } else {
            (
                Op::LoadElement,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            )
        };
        let after_element = if store {
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)])
        } else {
            (
                Op::Mul,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(8),
                ],
            )
        };
        let instructions = vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(10),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(7),
                ],
            ),
            (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(7)]),
            (
                Op::Call,
                vec![
                    Operand::Register(6),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(5), Operand::Imm32(41)],
            ),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(8), Operand::Imm32(2)]),
            element,
            after_element,
            (Op::LeaveTry, Vec::new()),
            (Op::ReturnValue, vec![Operand::Register(4)]),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ];
        let mut view = JitCompileSnapshot::without_feedback(
            91,
            2,
            9,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        view.seed_arith_feedback_for_test(8, ArithFeedback::from_bits(ARITH_INT32));
        let call_byte_pc = view.instructions[2].byte_pc;
        view.direct_callees.insert(
            call_byte_pc,
            vec![JitDirectCallee {
                plan: JitDirectCallPlan {
                    function_id: 92,
                    code_object_id: 1,
                    entry_cell: 1,
                    tier: NativeFrameKind::Baseline,
                    this_mode: JitDirectCallThisMode::StrictOrLexical,
                    is_derived_constructor: false,
                    generated_stack_frame_bytes: Some(0),
                    param_count: 0,
                    register_count: 1,
                    own_upvalue_count: 0,
                    inherited_upvalue_count: 0,
                    needs_incoming_arguments: false,
                },
                receiver_allocation: None,
            }],
        );
        view.cage_base = 0x1000;
        view.element_accesses.insert(
            view.instructions[7].byte_pc,
            JitElementAccess {
                type_tag: 1,
                base: JitElementBase::InBody { byte: 8 },
                ..JitElementAccess::default()
            },
        );
        view
    }

    fn catch_liveness_view() -> JitCompileSnapshot {
        catch_liveness_view_with_element_store(false)
    }

    fn direct_call_catch_view() -> JitCompileSnapshot {
        let mut view = JitCompileSnapshot::without_feedback(
            128,
            1,
            3,
            vec![
                JitTestInstruction::new(
                    Op::EnterTry,
                    0,
                    0,
                    vec![
                        Operand::Imm32(3),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(
                    Op::Call,
                    1,
                    8,
                    vec![
                        Operand::Register(1),
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                    ],
                ),
                JitTestInstruction::new(Op::LeaveTry, 2, 16, Vec::new()),
                JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(1)]),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(2)]),
            ],
        );
        view.direct_callees.insert(8, vec![direct_callee(129)]);
        view
    }

    fn property_view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(
            101,
            0,
            4,
            vec![
                JitTestInstruction::new(
                    Op::LoadInt32,
                    0,
                    0,
                    vec![Operand::Register(0), Operand::Imm32(7)],
                ),
                JitTestInstruction::new(
                    Op::LoadProperty,
                    1,
                    8,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::ConstIndex(9),
                    ],
                ),
                JitTestInstruction::new(Op::LoadTrue, 2, 16, vec![Operand::Register(1)]),
                JitTestInstruction::new(
                    Op::StoreProperty,
                    3,
                    24,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(10),
                        Operand::Register(1),
                        Operand::Register(3),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(2)]),
            ],
        )
    }

    fn property_catch_liveness_view() -> JitCompileSnapshot {
        let instructions = vec![
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(10),
                    Operand::Imm32(NO_HANDLER_OFFSET),
                    Operand::Register(7),
                ],
            ),
            (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(7)]),
            (
                Op::Call,
                vec![
                    Operand::Register(6),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(5), Operand::Imm32(41)],
            ),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::Nop, Vec::new()),
            (Op::Nop, Vec::new()),
            (
                Op::LoadProperty,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                ],
            ),
            (Op::Nop, Vec::new()),
            (Op::LeaveTry, Vec::new()),
            (Op::ReturnValue, vec![Operand::Register(3)]),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ];
        let mut view = JitCompileSnapshot::without_feedback(
            102,
            2,
            8,
            instructions
                .into_iter()
                .enumerate()
                .map(|(pc, (op, operands))| {
                    JitTestInstruction::new(op, pc as u32, pc as u32 * 8, operands)
                })
                .collect(),
        );
        let call_byte_pc = view.instructions[2].byte_pc;
        view.direct_callees.insert(
            call_byte_pc,
            vec![JitDirectCallee {
                plan: JitDirectCallPlan {
                    function_id: 103,
                    code_object_id: 1,
                    entry_cell: 1,
                    tier: NativeFrameKind::Baseline,
                    this_mode: JitDirectCallThisMode::StrictOrLexical,
                    is_derived_constructor: false,
                    generated_stack_frame_bytes: Some(0),
                    param_count: 0,
                    register_count: 1,
                    own_upvalue_count: 0,
                    inherited_upvalue_count: 0,
                    needs_incoming_arguments: false,
                },
                receiver_allocation: None,
            }],
        );
        view
    }

    #[test]
    fn unseen_plain_and_method_calls_build_exact_cold_exits() {
        for (method, expected_kind) in [
            (false, NumericColdCallKind::Plain),
            (true, NumericColdCallKind::Method),
        ] {
            let hir = NumericFunction::build(&call_view(method)).expect("cold call HIR");
            let (cold, logical_pc, byte_pc) = hir
                .nodes
                .iter()
                .enumerate()
                .find_map(|(index, node)| match node {
                    NumericNode::ColdCallExit {
                        kind,
                        logical_pc,
                        byte_pc,
                        ..
                    } => Some((NumericValue(index), (*kind, *logical_pc), *byte_pc)),
                    _ => None,
                })
                .map(|(value, (kind, logical_pc), byte_pc)| {
                    assert_eq!(kind, expected_kind);
                    (value, logical_pc, byte_pc)
                })
                .expect("cold call node");
            assert_eq!((logical_pc, byte_pc), (0, 0));
            assert!(hir.direct_call_targets.is_empty());
            assert!(hir.operand_values.is_empty());

            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(cold))
                .expect("exact pre-call state");
            let receiver = hir
                .nodes
                .iter()
                .enumerate()
                .find_map(|(index, node)| match node {
                    NumericNode::Parameter { register: 0, .. } => Some(NumericValue(index)),
                    _ => None,
                })
                .expect("receiver parameter");
            let argument = hir
                .nodes
                .iter()
                .enumerate()
                .find_map(|(index, node)| match node {
                    NumericNode::Parameter { register: 1, .. } => Some(NumericValue(index)),
                    _ => None,
                })
                .expect("argument parameter");
            assert_eq!(state.slots[0], NumericFrameSlot::Value(receiver));
            assert_eq!(state.slots[1], NumericFrameSlot::Value(argument));
            assert_eq!(state.slots[2], NumericFrameSlot::Undefined);
        }
    }

    #[test]
    fn attempted_unplanned_plain_and_method_calls_own_a_generic_final_miss() {
        // An attempted plain call the profile could not settle takes the
        // explicit-receiver value call with `undefined` as argument word zero.
        let mut plain = call_view(false);
        plain.seed_call_attempted_for_test(0);
        let hir = NumericFunction::build(&plain).expect("attempted generic plain HIR");
        let target = &hir.direct_call_targets[0];
        assert_eq!(target.kind, NumericDirectCallKind::CallWithThis);
        assert!(target.candidates.is_empty());
        let (start, count) = hir
            .nodes
            .iter()
            .find_map(|node| match node {
                NumericNode::DirectCall {
                    arguments: NumericDirectCallArguments::Fixed { start, count },
                    ..
                } => Some((*start as usize, *count)),
                _ => None,
            })
            .expect("generic plain call node");
        assert_eq!(count, 2, "receiver word plus one argument");
        assert!(matches!(
            hir.nodes[hir.operand_values[start].0],
            NumericNode::TaggedConstant(bits) if bits == otter_vm::Value::undefined().to_bits()
        ));
        assert!(
            hir.nodes
                .iter()
                .all(|node| !matches!(node, NumericNode::ColdCallExit { .. }))
        );

        let mut method = call_view_with_arguments(true, &[1, 1, 1, 1, 1, 1, 1, 1]);
        method.seed_call_attempted_for_test(0);
        let hir = NumericFunction::build(&method).expect("attempted generic method HIR");
        let target = &hir.direct_call_targets[0];
        assert_eq!(target.kind, NumericDirectCallKind::Method);
        assert!(target.candidates.is_empty());
        assert!(
            hir.nodes
                .iter()
                .all(|node| !matches!(node, NumericNode::ColdCallExit { .. }))
        );
        assert!(hir.nodes.iter().any(|node| matches!(
            node,
            NumericNode::DirectCall {
                arguments: NumericDirectCallArguments::Fixed { count: 8, .. },
                ..
            }
        )));
    }

    #[test]
    fn method_argument_metadata_is_u32_and_independent_of_target_count() {
        for argument_registers in [vec![1, 2, 3, 4, 5], (1..=8).collect::<Vec<_>>()] {
            let mut view = call_view_with_arguments(true, &argument_registers);
            view.seed_call_attempted_for_test(0);
            view.direct_methods
                .insert(0, vec![direct_method(0, 1, 150)]);
            let hir = NumericFunction::build(&view).expect("wide planned method HIR");
            assert!(hir.nodes.iter().any(|node| matches!(
                node,
                NumericNode::DirectCall {
                    arguments: NumericDirectCallArguments::Fixed { count, .. },
                    ..
                } if *count == argument_registers.len() as u32
            )));
            assert_eq!(hir.direct_call_targets[0].candidates.len(), 1);
        }

        // Wordcode itself bounds one instruction's operand vector to u8, but
        // HIR metadata is deliberately not coupled to that serialization
        // detail. Exercise the checked aggregate representation directly.
        let mut storage = Vec::new();
        let (start, count) =
            append_operand_values(&mut storage, std::iter::repeat_n(NumericValue(0), 300))
                .expect("argc > u8 HIR metadata");
        assert_eq!((start, count, storage.len()), (0, 300, 300));
    }

    #[test]
    fn direct_plan_wins_and_complete_method_chains_are_preserved() {
        let mut plain = call_view(false);
        plain.seed_call_attempted_for_test(0);
        plain.direct_callees.insert(0, vec![direct_callee(120)]);
        let hir = NumericFunction::build(&plain).expect("planned plain call HIR");
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::DirectCall { .. }))
        );
        assert!(
            hir.nodes
                .iter()
                .all(|node| !matches!(node, NumericNode::ColdCallExit { .. }))
        );
        assert_eq!(hir.direct_call_targets[0].candidates.len(), 1);
        assert!(hir.direct_call_targets[0].candidates[0].guard.is_none());

        for count in [1_u32, 2, 3, 4] {
            let mut view = call_view(true);
            view.seed_call_attempted_for_test(0);
            view.direct_methods.insert(
                0,
                (0..count)
                    .map(|index| direct_method(index, count, 130 + index))
                    .collect(),
            );
            let hir = NumericFunction::build(&view).expect("complete method chain HIR");
            let target = &hir.direct_call_targets[0];
            assert_eq!(target.kind, NumericDirectCallKind::Method);
            assert_eq!(target.candidates.len(), count as usize);
            for (index, candidate) in target.candidates.iter().enumerate() {
                assert_eq!(candidate.target_index, index as u32);
                assert_eq!(candidate.target_count, count);
                assert!(candidate.guard.is_some());
            }
        }
    }

    #[test]
    fn partial_gapped_or_oversized_method_chains_reject_machine_hir() {
        let mut wrong_function = direct_method(0, 1, 140);
        wrong_function.guard.method_fid += 1;
        let invalid = [
            Vec::new(),
            vec![direct_method(0, 2, 140)],
            vec![direct_method(0, 2, 140), direct_method(2, 2, 141)],
            vec![direct_method(0, 2, 140), direct_method(1, 3, 141)],
            vec![wrong_function],
            (0..5)
                .map(|index| direct_method(index, 5, 140 + index))
                .collect(),
        ];
        for methods in invalid {
            let mut view = call_view(true);
            view.direct_methods.insert(0, methods);
            assert!(NumericFunction::build(&view).is_err());
        }
    }

    #[test]
    fn array_construct_accepts_only_zero_or_one_exact_int32_length() {
        let zero = NumericFunction::build(&array_construct_view(0)).expect("zero-argument HIR");
        let (construct, length) = zero
            .nodes
            .iter()
            .enumerate()
            .find_map(|(index, node)| match node {
                NumericNode::ArrayConstruct { length, byte_pc: 0 } => {
                    Some((NumericValue(index), *length))
                }
                _ => None,
            })
            .expect("zero-argument array construct");
        assert_eq!(zero.nodes[length.0], NumericNode::IntegerConstant(0));
        assert_eq!(zero.nodes[construct.0].value_type(), NumericType::Tagged);
        assert_eq!(
            zero.frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(construct))
                .expect("exact zero-argument construction state")
                .byte_pc,
            0
        );

        let one = NumericFunction::build(&array_construct_view(1)).expect("one-argument HIR");
        let (construct, length) = one
            .nodes
            .iter()
            .enumerate()
            .find_map(|(index, node)| match node {
                NumericNode::ArrayConstruct { length, byte_pc: 0 } => {
                    Some((NumericValue(index), *length))
                }
                _ => None,
            })
            .expect("one-argument array construct");
        assert_eq!(one.nodes[length.0].value_type(), NumericType::Int32);
        let state = one
            .frame_states
            .iter()
            .find(|state| state.point == NumericFramePoint::Node(construct))
            .expect("exact one-argument construction state");
        assert_eq!(state.byte_pc, 0);
        assert_eq!(state.slots[0], NumericFrameSlot::Value(length));
        assert_eq!(state.slots[2], NumericFrameSlot::Undefined);

        assert!(
            NumericFunction::build(&array_construct_view(2)).is_err(),
            "wider Array construction must retain the Template baseline"
        );
    }

    #[test]
    fn tagged_array_length_decode_and_construct_share_exact_pre_operation_state() {
        let hir =
            NumericFunction::build(&tagged_array_construct_view()).expect("tagged length HIR");
        let decode = hir
            .nodes
            .iter()
            .position(|node| matches!(node, NumericNode::TaggedToInt32(_)))
            .map(NumericValue)
            .expect("exact tagged Int32 decode");
        let construct = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::ArrayConstruct {
                        length,
                        byte_pc: 8
                    } if *length == decode
                )
            })
            .map(NumericValue)
            .expect("array construct consuming the decoded length");
        let property = match hir.nodes[decode.0] {
            NumericNode::TaggedToInt32(source) => source,
            _ => unreachable!("matched tagged decode"),
        };

        for point in [decode, construct] {
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(point))
                .expect("exact pre-construction state");
            assert_eq!(state.byte_pc, 8);
            assert_eq!(state.slots[1], NumericFrameSlot::Value(property));
            assert_eq!(state.slots[2], NumericFrameSlot::Undefined);
        }
    }

    #[test]
    fn global_binding_proofs_build_one_typed_family_with_exact_gc_state() {
        let lexical_target = BindingHitProof::GlobalLexical {
            cell_offset: 0x88,
            writable: true,
        };
        let object_target = BindingHitProof::GlobalObject {
            shape: 0x1234,
            dictionary: true,
            value_byte: 40,
            global_lexical_epoch: 9,
            writable: true,
        };

        for target in [lexical_target, object_target] {
            let mut view = global_load_view();
            view.cage_base = 0x1000;
            view.binding_hit_proofs.insert(24, target);
            let hir = NumericFunction::build(&view).expect("typed global-binding HIR");
            let global = hir
                .nodes
                .iter()
                .position(|node| {
                    matches!(
                        node,
                        NumericNode::Binding {
                            semantics: BindingSemantics::Read(BindingRead::Global { .. }),
                            target: Some(NumericBindingTarget::Global(copied)),
                            byte_pc: 24,
                            ..
                        } if *copied == target
                    )
                })
                .map(NumericValue)
                .expect("copied binding proof");
            assert_eq!(hir.nodes[global.0].value_type(), NumericType::Tagged);

            let parameter = hir
                .nodes
                .iter()
                .position(|node| {
                    matches!(
                        node,
                        NumericNode::Parameter {
                            register: 0,
                            value_type: NumericType::Tagged
                        }
                    )
                })
                .map(NumericValue)
                .expect("live entry parameter");
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(global))
                .expect("exact pre-binding GC state");
            assert_eq!(state.byte_pc, 24);
            assert_eq!(state.slots[0], NumericFrameSlot::Value(parameter));
            assert_eq!(state.slots[1], NumericFrameSlot::Undefined);
        }
    }

    #[test]
    fn prepared_string_literal_builds_a_pure_tagged_cell_load() {
        let target = JitStringConstantCell { cell_addr: 0x1234 };
        let mut view = JitCompileSnapshot::without_feedback(
            42,
            0,
            1,
            vec![
                JitTestInstruction::new(
                    Op::LoadString,
                    0,
                    24,
                    vec![Operand::Register(0), Operand::ConstIndex(0)],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 32, vec![Operand::Register(0)]),
            ],
        );
        assert!(
            NumericFunction::build(&view).is_err(),
            "an unprepared literal must keep the function on Template"
        );

        view.string_constant_cells.insert(24, target);
        let hir = NumericFunction::build(&view).expect("prepared string HIR");
        let literal = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::StringConstantCell {
                        byte_pc: 24,
                        target: copied,
                    } if *copied == target
                )
            })
            .map(NumericValue)
            .expect("string stable-cell node");
        assert_eq!(hir.nodes[literal.0].value_type(), NumericType::Tagged);
        assert!(
            hir.frame_states
                .iter()
                .all(|state| state.point != NumericFramePoint::Node(literal)),
            "a leaf stable-cell load owns neither deopt state nor GC roots"
        );
    }

    #[test]
    fn global_binding_without_proof_keeps_committed_cold_sibling() {
        let mut view = global_load_view();
        let hir = NumericFunction::build(&view).expect("cold global-binding HIR");
        assert!(hir.nodes.iter().any(|node| {
            matches!(
                node,
                NumericNode::Binding {
                    semantics: BindingSemantics::Read(BindingRead::Global { .. }),
                    target: None,
                    byte_pc: 24,
                    ..
                }
            )
        }));

        let lexical_target = BindingHitProof::GlobalLexical {
            cell_offset: 0x90,
            writable: true,
        };
        view.cage_base = 0x1000;
        view.binding_hit_proofs.insert(24, lexical_target);
        let hir = NumericFunction::build(&view).expect("proved global-binding HIR");
        assert!(hir.nodes.iter().any(|node| {
            matches!(
                node,
                NumericNode::Binding {
                    semantics: BindingSemantics::Read(BindingRead::Global { .. }),
                    target: Some(NumericBindingTarget::Global(target)),
                    byte_pc: 24,
                    ..
                } if *target == lexical_target
            )
        }));

        for proof in [
            BindingHitProof::GlobalLexical {
                cell_offset: 0x98,
                writable: true,
            },
            BindingHitProof::GlobalObject {
                shape: 5,
                dictionary: false,
                value_byte: 16,
                global_lexical_epoch: 2,
                writable: true,
            },
        ] {
            let mut no_cage = global_load_view();
            no_cage.binding_hit_proofs.insert(24, proof);
            let hir = NumericFunction::build(&no_cage).expect("cold no-cage binding HIR");
            assert!(hir.nodes.iter().any(|node| {
                matches!(
                    node,
                    NumericNode::Binding {
                        byte_pc: 24,
                        target: None,
                        ..
                    }
                )
            }));
        }
    }

    #[test]
    fn global_load_inference_accesses_and_exception_liveness_are_exact() {
        let view = global_load_view();
        let code = view.code_block.as_ref();
        let mut origins = vec![1, 2];
        let mut int32_parameters = 0;
        let mut number_parameters = 0;
        infer_instruction_parameters(
            &view.instructions[0],
            code,
            &mut origins,
            &mut int32_parameters,
            &mut number_parameters,
        )
        .expect("global-load inference");
        assert_eq!(origins, [1, 0]);
        assert_eq!(int32_parameters, 0);
        assert_eq!(number_parameters, 0);
        assert_eq!(
            instruction_accesses(&view.instructions[0], code),
            Some((Vec::new(), vec![1]))
        );

        let mut live = vec![false; 3];
        live[1] = true;
        let mut catch_live = vec![false; 3];
        catch_live[0] = true;
        catch_live[2] = true;
        let semantics = classify_snapshot(&view).expect("global-load semantics");
        transfer_instruction_liveness(
            &view.instructions[0],
            code,
            Some(InstructionExceptionHandler {
                block: 0,
                exception_register: 2,
            }),
            &[catch_live],
            semantics[0],
            &mut live,
        )
        .expect("implicit global-load exception liveness");
        assert!(semantics[0].has_implicit_exception_side_exit(Op::LoadGlobalOrThrow));
        assert!(live[0], "catch-only state remains live at the exact exit");
        assert!(!live[1], "the destination is killed before the operation");
        assert!(!live[2], "the catch supplies its exception register");
    }

    #[test]
    fn schema_only_value_ops_use_the_generic_admission_transfer() {
        let view = JitCompileSnapshot::without_feedback(
            122,
            2,
            4,
            vec![
                JitTestInstruction::new(
                    Op::LoadString,
                    0,
                    0,
                    vec![Operand::Register(2), Operand::ConstIndex(0)],
                ),
                JitTestInstruction::new(
                    Op::Instanceof,
                    1,
                    8,
                    vec![
                        Operand::Register(3),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(
                    Op::StoreGlobalBinding,
                    2,
                    16,
                    vec![
                        Operand::Register(3),
                        Operand::ConstIndex(1),
                        Operand::Imm32(0),
                    ],
                ),
                JitTestInstruction::new(
                    Op::StoreUpvalueChecked,
                    3,
                    24,
                    vec![Operand::Register(0), Operand::Imm32(0)],
                ),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(3)]),
            ],
        );
        let code = view.code_block.as_ref();
        assert_eq!(
            instruction_accesses(&view.instructions[0], code),
            Some((Vec::new(), vec![2]))
        );
        assert_eq!(
            instruction_accesses(&view.instructions[1], code),
            Some((vec![0, 1], vec![3]))
        );
        assert_eq!(
            instruction_accesses(&view.instructions[2], code),
            Some((vec![3], Vec::new()))
        );
        assert_eq!(
            instruction_accesses(&view.instructions[3], code),
            Some((vec![0], Vec::new()))
        );

        let mut origins = [1, 2, 4, 8];
        let mut int32_parameters = 0;
        let mut number_parameters = 0;
        for instruction in &view.instructions[..4] {
            infer_instruction_parameters(
                instruction,
                code,
                &mut origins,
                &mut int32_parameters,
                &mut number_parameters,
            )
            .expect("schema-declared value operation must not need an inference mirror");
        }
        assert_eq!(origins, [1, 2, 0, 0]);
        assert_eq!(int32_parameters, 0);
        assert_eq!(number_parameters, 0);
    }

    #[test]
    fn committed_value_family_is_admitted_by_one_schema_driven_lowering() {
        let cases = vec![
            (
                Op::Instanceof,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
                Some(2),
                2,
            ),
            (
                Op::HasProperty,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
                Some(2),
                2,
            ),
            (
                Op::GetPrototype,
                vec![Operand::Register(2), Operand::Register(0)],
                Some(2),
                1,
            ),
            (
                Op::SetPrototype,
                vec![Operand::Register(0), Operand::Register(1)],
                None,
                2,
            ),
            (
                Op::ToObject,
                vec![Operand::Register(2), Operand::Register(0)],
                Some(2),
                1,
            ),
            (
                Op::ToPropertyKey,
                vec![Operand::Register(2), Operand::Register(0)],
                Some(2),
                1,
            ),
            (
                Op::TypeOf,
                vec![Operand::Register(2), Operand::Register(0)],
                Some(2),
                1,
            ),
            (Op::LoadNewTarget, vec![Operand::Register(2)], Some(2), 0),
            (
                Op::SameValue,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
                Some(2),
                2,
            ),
            (Op::BindThisValue, vec![Operand::Register(0)], None, 1),
        ];
        for (op, operands, destination, expected_arity) in cases {
            let tail = destination.map_or_else(
                || JitTestInstruction::new(Op::ReturnUndefined, 1, 8, Vec::new()),
                |destination| {
                    JitTestInstruction::new(
                        Op::ReturnValue,
                        1,
                        8,
                        vec![Operand::Register(destination)],
                    )
                },
            );
            let view = JitCompileSnapshot::without_feedback(
                124,
                2,
                3,
                vec![JitTestInstruction::new(op, 0, 0, operands), tail],
            );
            let hir = NumericFunction::build(&view)
                .unwrap_or_else(|_| panic!("{op:?} must enter generic committed HIR"));
            let committed = hir
                .nodes
                .iter()
                .find_map(|node| match node {
                    NumericNode::CommittedValue { inputs, .. } => Some(inputs),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{op:?} committed node"));
            assert_eq!(committed.iter().flatten().count(), expected_arity, "{op:?}");
        }
    }

    #[test]
    fn tagged_loose_nullish_equality_keeps_direction_and_exact_pre_operation_state() {
        for (op, equal) in [(Op::LooseEqual, true), (Op::LooseNotEqual, false)] {
            for literal in [Op::LoadNull, Op::LoadUndefined] {
                for literal_left in [false, true] {
                    let hir =
                        NumericFunction::build(&loose_nullish_view(op, literal, literal_left))
                            .expect("tagged nullish equality HIR");
                    let parameter = hir
                        .nodes
                        .iter()
                        .position(|node| {
                            matches!(
                                node,
                                NumericNode::Parameter {
                                    register: 0,
                                    value_type: NumericType::Tagged
                                }
                            )
                        })
                        .map(NumericValue)
                        .expect("dynamic tagged operand");
                    let comparison = hir
                        .nodes
                        .iter()
                        .position(|node| {
                            matches!(
                                node,
                                NumericNode::TaggedNullishEqual {
                                    value,
                                    equal: node_equal,
                                    byte_pc: 8,
                                } if *value == parameter && *node_equal == equal
                            )
                        })
                        .map(NumericValue)
                        .expect("direction-preserving nullish comparison");
                    assert_eq!(hir.nodes[comparison.0].value_type(), NumericType::Boolean);
                    let state = hir
                        .frame_states
                        .iter()
                        .find(|state| state.point == NumericFramePoint::Node(comparison))
                        .expect("exact pre-nullish-comparison state");
                    assert_eq!(state.byte_pc, 8);
                    assert_eq!(state.slots[0], NumericFrameSlot::Value(parameter));
                    assert_eq!(state.slots[2], NumericFrameSlot::Undefined);
                }
            }
        }
    }

    #[test]
    fn two_static_nullish_operands_fold_without_an_htmldda_exit() {
        for (op, expected) in [(Op::LooseEqual, true), (Op::LooseNotEqual, false)] {
            let view = JitCompileSnapshot::without_feedback(
                118,
                0,
                3,
                vec![
                    JitTestInstruction::new(Op::LoadNull, 0, 0, vec![Operand::Register(0)]),
                    JitTestInstruction::new(Op::LoadUndefined, 1, 8, vec![Operand::Register(1)]),
                    JitTestInstruction::new(
                        op,
                        2,
                        16,
                        vec![
                            Operand::Register(2),
                            Operand::Register(0),
                            Operand::Register(1),
                        ],
                    ),
                    JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(2)]),
                ],
            );
            let hir = NumericFunction::build(&view).expect("constant nullish equality HIR");
            assert!(hir.nodes.contains(&NumericNode::BooleanConstant(expected)));
            assert!(
                hir.nodes
                    .iter()
                    .all(|node| !matches!(node, NumericNode::TaggedNullishEqual { .. }))
            );
        }
    }

    #[test]
    fn loose_numeric_equality_reuses_integer_and_float_comparison_nodes() {
        for op in [Op::LooseEqual, Op::LooseNotEqual] {
            let int_hir = NumericFunction::build(&loose_numeric_view(
                op,
                ArithFeedback::from_bits(ARITH_INT32),
            ))
            .expect("Int32 loose equality HIR");
            assert!(int_hir.nodes.iter().any(|node| match *node {
                NumericNode::IntegerEqual(left, right) if op == Op::LooseEqual => {
                    int_hir.nodes[left.0].value_type() == NumericType::Int32
                        && int_hir.nodes[right.0].value_type() == NumericType::Int32
                }
                NumericNode::IntegerNotEqual(left, right) if op == Op::LooseNotEqual => {
                    int_hir.nodes[left.0].value_type() == NumericType::Int32
                        && int_hir.nodes[right.0].value_type() == NumericType::Int32
                }
                _ => false,
            }));

            let float_hir = NumericFunction::build(&loose_numeric_view(
                op,
                ArithFeedback::from_bits(ARITH_INT32 | ARITH_FLOAT64),
            ))
            .expect("Float64 loose equality HIR");
            assert!(float_hir.nodes.iter().any(|node| match *node {
                NumericNode::Equal(left, right) if op == Op::LooseEqual => {
                    float_hir.nodes[left.0].value_type() == NumericType::Number
                        && float_hir.nodes[right.0].value_type() == NumericType::Number
                }
                NumericNode::NotEqual(left, right) if op == Op::LooseNotEqual => {
                    float_hir.nodes[left.0].value_type() == NumericType::Number
                        && float_hir.nodes[right.0].value_type() == NumericType::Number
                }
                _ => false,
            }));
        }
    }

    #[test]
    fn unseen_loose_equality_guards_numeric_inputs_and_declines_malformed_shapes() {
        let generic = JitCompileSnapshot::without_feedback(
            116,
            2,
            3,
            vec![
                JitTestInstruction::new(
                    Op::LooseEqual,
                    0,
                    0,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(2)]),
            ],
        );
        let hir = NumericFunction::build(&generic).expect("guarded unseen loose-equality HIR");
        let decodes = hir
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                matches!(node, NumericNode::TaggedToNumber(_)).then_some(NumericValue(index))
            })
            .collect::<Vec<_>>();
        assert_eq!(decodes.len(), 2);
        assert!(
            hir.nodes
                .iter()
                .any(|node| matches!(node, NumericNode::Equal(_, _)))
        );
        for decode in decodes {
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(decode))
                .expect("exact pre-coercion state");
            assert_eq!(state.byte_pc, 0);
            assert_eq!(state.slots[2], NumericFrameSlot::Undefined);
        }

        for operands in [
            vec![
                Operand::Register(9),
                Operand::Register(0),
                Operand::Register(1),
            ],
            vec![
                Operand::Register(2),
                Operand::Register(9),
                Operand::Register(1),
            ],
            vec![
                Operand::Register(2),
                Operand::Register(0),
                Operand::Register(9),
            ],
        ] {
            let malformed = JitCompileSnapshot::without_feedback(
                117,
                2,
                3,
                vec![
                    JitTestInstruction::new(Op::LooseNotEqual, 0, 0, operands),
                    JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(2)]),
                ],
            );
            assert!(NumericFunction::build(&malformed).is_err());
        }
    }

    #[test]
    fn loose_equality_snapshot_path_has_no_semantic_exception_edge() {
        let view = loose_nullish_view(Op::LooseEqual, Op::LoadNull, false);
        let code = view.code_block.as_ref();
        let comparison = &view.instructions[1];
        let mut origins = vec![1, 2, 4];
        let mut int32_parameters = 0;
        let mut number_parameters = 0;
        infer_instruction_parameters(
            comparison,
            code,
            &mut origins,
            &mut int32_parameters,
            &mut number_parameters,
        )
        .expect("loose-equality inference");
        assert_eq!(origins, [1, 2, 0]);
        assert_eq!(
            instruction_accesses(comparison, code),
            Some((vec![0, 1], vec![2]))
        );

        let mut live = vec![false; 5];
        live[2] = true;
        let mut catch_live = vec![false; 5];
        catch_live[3] = true;
        catch_live[4] = true;
        let semantics = classify_snapshot(&view).expect("loose-equality semantics");
        transfer_instruction_liveness(
            comparison,
            code,
            Some(InstructionExceptionHandler {
                block: 0,
                exception_register: 3,
            }),
            &[catch_live],
            semantics[1],
            &mut live,
        )
        .expect("snapshot-aware loose-equality liveness");
        assert!(!semantics[1].has_implicit_exception_side_exit(Op::LooseEqual));
        assert!(live[0] && live[1], "both operands are ordinary reads");
        assert!(!live[2], "destination is killed before the operation");
        assert!(!live[3], "the catch supplies its exception register");
        assert!(
            !live[4],
            "a proved pre-effect path does not fabricate a semantic throw edge"
        );
    }

    #[test]
    fn unseen_immediate_comparisons_decode_tagged_numbers_at_the_exact_site() {
        for op in [Op::LessThanImm, Op::EqualImm, Op::NotEqualImm] {
            let hir = NumericFunction::build(&unseen_immediate_numeric_view(op))
                .expect("guarded unseen immediate comparison HIR");
            let parameter = hir
                .nodes
                .iter()
                .position(|node| {
                    matches!(
                        node,
                        NumericNode::Parameter {
                            register: 0,
                            value_type: NumericType::Tagged
                        }
                    )
                })
                .map(NumericValue)
                .expect("unseen input remains tagged");
            let decode = hir
                .nodes
                .iter()
                .position(|node| *node == NumericNode::TaggedToNumber(parameter))
                .map(NumericValue)
                .expect("exact tagged-number decode");
            let immediate = hir
                .nodes
                .iter()
                .position(|node| *node == NumericNode::Constant(7.0))
                .map(NumericValue)
                .expect("Float64 immediate");
            assert!(hir.nodes.iter().any(|node| match (op, node) {
                (Op::LessThanImm, NumericNode::LessThan(left, right))
                | (Op::EqualImm, NumericNode::Equal(left, right))
                | (Op::NotEqualImm, NumericNode::NotEqual(left, right)) => {
                    (*left, *right) == (decode, immediate)
                }
                _ => false,
            }));

            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(decode))
                .expect("exact pre-comparison decode state");
            assert_eq!(state.byte_pc, 24);
            assert_eq!(state.slots[0], NumericFrameSlot::Value(parameter));
            assert_eq!(state.slots[1], NumericFrameSlot::Undefined);
        }
    }

    #[test]
    fn unseen_division_guards_both_tagged_operands_without_entry_specialization() {
        let hir = NumericFunction::build(&unseen_div_view()).expect("guarded unseen division HIR");
        let parameters = (0..2_u16)
            .map(|register| {
                hir.nodes
                    .iter()
                    .position(|node| {
                        matches!(
                            node,
                            NumericNode::Parameter {
                                register: node_register,
                                value_type: NumericType::Tagged
                            } if *node_register == register
                        )
                    })
                    .map(NumericValue)
                    .expect("unseen division parameter remains tagged")
            })
            .collect::<Vec<_>>();
        let decodes = parameters
            .iter()
            .map(|&parameter| {
                hir.nodes
                    .iter()
                    .position(|node| *node == NumericNode::TaggedToNumber(parameter))
                    .map(NumericValue)
                    .expect("per-operand exact numeric decode")
            })
            .collect::<Vec<_>>();
        assert!(
            hir.nodes
                .iter()
                .any(|node| *node == NumericNode::Div(decodes[0], decodes[1]))
        );
        for &decode in &decodes {
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(decode))
                .expect("exact pre-division decode state");
            assert_eq!(state.byte_pc, 40);
            assert_eq!(state.slots[0], NumericFrameSlot::Value(parameters[0]));
            assert_eq!(state.slots[1], NumericFrameSlot::Value(parameters[1]));
            assert_eq!(state.slots[2], NumericFrameSlot::Undefined);
        }
    }

    #[test]
    fn unseen_number_product_remains_an_admissible_element_index() {
        let hir =
            NumericFunction::build(&number_index_element_view()).expect("Number-index element HIR");
        let product = hir
            .nodes
            .iter()
            .position(|node| matches!(node, NumericNode::Mul(..)))
            .map(NumericValue)
            .expect("guarded Float64 product");
        assert_eq!(hir.nodes[product.0].value_type(), NumericType::Number);

        let load = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::ElementLoad {
                        index,
                        byte_pc: 8,
                        ..
                    } if *index == product
                )
            })
            .map(NumericValue)
            .expect("Number-index element load");
        let store = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::ElementStore {
                        index,
                        byte_pc: 16,
                        ..
                    } if *index == product
                )
            })
            .map(NumericValue)
            .expect("Number-index element store");
        for (point, byte_pc) in [(load, 8), (store, 16)] {
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(point))
                .expect("exact pre-element Number-index state");
            assert_eq!(state.byte_pc, byte_pc);
            assert_eq!(state.slots[3], NumericFrameSlot::Value(product));
        }
    }

    #[test]
    fn packed_double_elements_keep_payloads_unboxed_and_check_number_indices() {
        let mut incomplete = packed_double_number_index_element_view();
        incomplete
            .element_accesses
            .get_mut(&8)
            .expect("packed load access")
            .guards[1] = None;
        let incomplete = NumericFunction::build(&incomplete)
            .expect("malformed direct metadata keeps the canonical generic element path");
        assert!(
            incomplete
                .nodes
                .iter()
                .any(|node| matches!(node, NumericNode::GenericElementLoad { byte_pc: 8, .. })),
            "raw Float64 InBody storage must never select the direct packed path"
        );

        let hir = NumericFunction::build(&packed_double_number_index_element_view())
            .expect("PackedDouble Number-index element HIR");
        let product = hir
            .nodes
            .iter()
            .position(|node| matches!(node, NumericNode::Mul(..)))
            .map(NumericValue)
            .expect("guarded Float64 product");
        let checks = hir
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match node {
                NumericNode::CheckedFloat64ToElementIndex { value, byte_pc }
                    if *value == product =>
                {
                    Some((NumericValue(index), *byte_pc))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            checks
                .iter()
                .map(|(_, byte_pc)| *byte_pc)
                .collect::<Vec<_>>(),
            [8, 16]
        );
        for &(check, byte_pc) in &checks {
            assert_eq!(hir.nodes[check.0].value_type(), NumericType::Uint32);
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(check))
                .expect("exact pre-element index conversion state");
            assert_eq!(state.byte_pc, byte_pc);
            assert_eq!(state.slots[3], NumericFrameSlot::Value(product));
        }

        let load = hir
            .nodes
            .iter()
            .enumerate()
            .find_map(|(index, node)| match node {
                NumericNode::ElementLoad {
                    index: checked,
                    access: NumericElementAccess::PackedDouble,
                    ..
                } => Some((NumericValue(index), *checked)),
                _ => None,
            })
            .expect("PackedDouble element load");
        assert_eq!(load.1, checks[0].0);
        assert_eq!(hir.nodes[load.0.0].value_type(), NumericType::Number);
        assert!(
            !hir.nodes.contains(&NumericNode::TaggedToNumber(load.0)),
            "the packed load must not immediately decode its own result"
        );

        let store = hir
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    node,
                    NumericNode::ElementStore {
                        index,
                        value,
                        access: NumericElementAccess::PackedDouble,
                        ..
                    } if *index == checks[1].0 && *value == load.0
                )
            })
            .expect("PackedDouble element store");
        assert_eq!(store.value_type(), NumericType::Tagged);
    }

    #[test]
    fn unprepared_elements_use_generic_value_calls_but_local_catches_stay_on_template_baseline() {
        let mut view = number_index_element_view();
        view.element_accesses.clear();
        let hir = NumericFunction::build(&view).expect("generic element HIR");
        let load = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::GenericElementLoad {
                        logical_pc: 1,
                        byte_pc: 8,
                        ..
                    }
                )
            })
            .map(NumericValue)
            .expect("generic element load");
        let store = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::GenericElementStore {
                        logical_pc: 2,
                        byte_pc: 16,
                        ..
                    }
                )
            })
            .map(NumericValue)
            .expect("generic element store");
        for (point, byte_pc) in [(load, 8), (store, 16)] {
            let state = hir
                .frame_states
                .iter()
                .find(|state| state.point == NumericFramePoint::Node(point))
                .expect("generic element pre-operation state");
            assert_eq!(state.byte_pc, byte_pc);
        }

        let mut caught = catch_liveness_view();
        caught.element_accesses.clear();
        assert!(
            NumericFunction::build(&caught).is_err(),
            "a generic value call inside a local catch stays on the Template baseline"
        );

        for store in [false, true] {
            let mut caught = catch_liveness_view_with_element_store(store);
            assert!(
                NumericFunction::build(&caught).is_err(),
                "a prepared tagged element committed miss inside a local catch stays on the Template baseline"
            );

            let byte_pc = caught.instructions[7].byte_pc;
            caught
                .element_accesses
                .insert(byte_pc, JitElementAccess::packed_double_array());
            assert!(
                NumericFunction::build(&caught).is_err(),
                "a may-throw element needs an explicit committed exception value before entering a local catch"
            );
        }
    }

    #[test]
    fn may_throw_store_receiver_is_never_used_as_the_exception_value() {
        let mut caught = catch_liveness_view_with_element_store(true);
        let byte_pc = caught.instructions[7].byte_pc;
        caught
            .element_accesses
            .insert(byte_pc, JitElementAccess::packed_double_array());
        assert!(
            NumericFunction::build(&caught).is_err(),
            "StoreElement operand zero is its receiver, not a NativeResultPair exception result"
        );
    }

    #[test]
    fn committed_bind_this_publishes_its_status_value_to_the_catch() {
        let view = JitCompileSnapshot::without_feedback(
            123,
            1,
            2,
            vec![
                JitTestInstruction::new(
                    Op::EnterTry,
                    0,
                    0,
                    vec![
                        Operand::Imm32(3),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::BindThisValue, 1, 8, vec![Operand::Register(0)]),
                JitTestInstruction::new(Op::LeaveTry, 2, 16, Vec::new()),
                JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(0)]),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(1)]),
            ],
        );
        let hir = NumericFunction::build(&view).expect("committed BindThis catch HIR");
        let bind = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::CommittedValue {
                        operation: CommittedValueOperation::Scalar(
                            otter_vm::native_abi::ScalarValueOp::BindThisValue
                        ),
                        ..
                    }
                )
            })
            .map(NumericValue)
            .expect("BindThis NativeResultPair value");
        let source = hir
            .blocks
            .iter()
            .position(|block| block.nodes.contains(&bind))
            .expect("BindThis source block");
        let handler = hir
            .blocks
            .iter()
            .position(|block| block.logical_pc == 4)
            .expect("catch block");
        let edge = hir.blocks[source]
            .successors
            .iter()
            .position(|&successor| successor == handler)
            .expect("BindThis exceptional edge");
        assert!(matches!(
            hir.nodes[bind.0],
            NumericNode::CommittedValue {
                exceptional_edge: Some(exceptional_edge),
                ..
            } if usize::from(exceptional_edge) == edge
        ));
        let parameter_index = hir.blocks[handler]
            .parameter_registers
            .iter()
            .position(|&register| register == 1)
            .expect("explicit exception parameter");
        let exception = hir.blocks[handler].parameters[parameter_index];
        assert!(matches!(
            hir.nodes[exception.0],
            NumericNode::BlockParameter(NumericType::Tagged)
        ));
        assert_eq!(
            hir.blocks[source].successor_arguments[edge][parameter_index],
            bind
        );
        assert_eq!(
            hir.blocks[handler].terminator,
            NumericTerminator::Return(exception),
            "the catch consumes its explicit committed exception parameter"
        );
    }

    #[test]
    fn committed_instanceof_publishes_a_pure_exception_ssa_landing() {
        let view = JitCompileSnapshot::without_feedback(
            125,
            2,
            4,
            vec![
                JitTestInstruction::new(
                    Op::EnterTry,
                    0,
                    0,
                    vec![
                        Operand::Imm32(3),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(3),
                    ],
                ),
                JitTestInstruction::new(
                    Op::Instanceof,
                    1,
                    8,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::LeaveTry, 2, 16, Vec::new()),
                JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(2)]),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(3)]),
            ],
        );
        let hir = NumericFunction::build(&view).expect("committed Instanceof catch HIR");
        let instanceof = hir
            .nodes
            .iter()
            .position(|node| {
                matches!(
                    node,
                    NumericNode::CommittedValue {
                        operation: CommittedValueOperation::ObjectProtocol(
                            otter_vm::native_abi::ObjectProtocolValueOp::Instanceof
                        ),
                        exceptional_edge: Some(_),
                        ..
                    }
                )
            })
            .map(NumericValue)
            .expect("Instanceof committed NativeResultPair");
        let source = hir
            .blocks
            .iter()
            .position(|block| block.nodes.contains(&instanceof))
            .expect("Instanceof source block");
        let handler = hir
            .blocks
            .iter()
            .position(|block| block.logical_pc == 4)
            .expect("catch block");
        let edge = hir.blocks[source]
            .successors
            .iter()
            .position(|&successor| successor == handler)
            .expect("pure exceptional edge");
        assert!(matches!(
            hir.nodes[instanceof.0],
            NumericNode::CommittedValue {
                exceptional_edge: Some(exceptional_edge),
                ..
            } if usize::from(exceptional_edge) == edge
        ));
        let parameter_index = hir.blocks[handler]
            .parameter_registers
            .iter()
            .position(|&register| register == 3)
            .expect("explicit exception parameter");
        let exception = hir.blocks[handler].parameters[parameter_index];
        assert!(matches!(
            hir.nodes[exception.0],
            NumericNode::BlockParameter(NumericType::Tagged)
        ));
        assert_eq!(
            hir.blocks[source].successor_arguments[edge][parameter_index],
            instanceof
        );
        assert_eq!(
            hir.blocks[handler].terminator,
            NumericTerminator::Return(exception),
            "the catch consumes its explicit pure-exception parameter"
        );
    }

    #[test]
    fn generated_direct_call_inside_local_catch_has_a_pure_exception_ssa_edge() {
        let hir = NumericFunction::build(&direct_call_catch_view())
            .expect("generated direct calls feed catch-only handlers through pure SSA");
        let direct_call = hir
            .nodes
            .iter()
            .position(|node| matches!(node, NumericNode::DirectCall { .. }))
            .map(NumericValue)
            .expect("prepared direct call");
        let source = hir
            .blocks
            .iter()
            .position(|block| block.nodes.contains(&direct_call))
            .expect("direct-call source block");
        let handler = hir
            .blocks
            .iter()
            .position(|block| block.logical_pc == 4)
            .expect("catch block");
        let edge = hir.blocks[source]
            .successors
            .iter()
            .position(|&successor| successor == handler)
            .expect("direct-call exceptional edge");
        assert!(matches!(
            hir.nodes[direct_call.0],
            NumericNode::DirectCall {
                exceptional_edge: Some(exceptional_edge),
                ..
            } if usize::from(exceptional_edge) == edge
        ));
        let parameter_index = hir.blocks[handler]
            .parameter_registers
            .iter()
            .position(|&register| register == 2)
            .expect("explicit exception parameter");
        let exception = hir.blocks[handler].parameters[parameter_index];
        assert!(matches!(
            hir.nodes[exception.0],
            NumericNode::BlockParameter(NumericType::Tagged)
        ));
        assert_eq!(
            hir.blocks[source].successor_arguments[edge][parameter_index],
            direct_call
        );
        assert_eq!(
            hir.blocks[handler].terminator,
            NumericTerminator::Return(exception),
            "the catch consumes only the direct call's explicit exception parameter"
        );
    }

    #[test]
    fn guarded_warm_int32_operation_inside_catch_region_is_machine_eligible() {
        let mut view = JitCompileSnapshot::without_feedback(
            126,
            2,
            4,
            vec![
                JitTestInstruction::new(
                    Op::EnterTry,
                    0,
                    0,
                    vec![
                        Operand::Imm32(3),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(3),
                    ],
                ),
                JitTestInstruction::new(
                    Op::Add,
                    1,
                    8,
                    vec![
                        Operand::Register(2),
                        Operand::Register(0),
                        Operand::Register(1),
                    ],
                ),
                JitTestInstruction::new(Op::LeaveTry, 2, 16, Vec::new()),
                JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(2)]),
                JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(3)]),
            ],
        );
        view.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
        let hir = NumericFunction::build(&view)
            .expect("catch-only handlers are reconstructed after an exact numeric deopt");
        assert!(hir.nodes.iter().any(|node| {
            matches!(
                node.frame_state_purpose(),
                Some(NumericFrameStatePurpose::ExactDeopt)
            )
        }));
    }

    #[test]
    fn metadata_only_frame_state_is_not_misclassified_as_exact_deopt() {
        let view = JitCompileSnapshot::without_feedback(
            127,
            0,
            1,
            vec![
                JitTestInstruction::new(
                    Op::EnterTry,
                    0,
                    0,
                    vec![
                        Operand::Imm32(3),
                        Operand::Imm32(NO_HANDLER_OFFSET),
                        Operand::Register(0),
                    ],
                ),
                JitTestInstruction::new(Op::Nop, 1, 8, Vec::new()),
                JitTestInstruction::new(Op::LeaveTry, 2, 16, Vec::new()),
                JitTestInstruction::new(Op::ReturnUndefined, 3, 24, Vec::new()),
                JitTestInstruction::new(Op::ReturnUndefined, 4, 32, Vec::new()),
            ],
        );
        let node = NumericNode::GenericElementLoad {
            receiver: NumericValue(1),
            index: NumericValue(2),
            logical_pc: 1,
            byte_pc: 8,
        };
        assert_eq!(
            node.frame_state_purpose(),
            Some(NumericFrameStatePurpose::RuntimeMetadata)
        );
        let hir = NumericFunction::build(&view).expect("metadata-only protected frame state");
        assert!(hir.nodes.iter().all(|node| {
            node.frame_state_purpose() != Some(NumericFrameStatePurpose::ExactDeopt)
        }));
    }

    #[test]
    fn packed_double_view_cache_groups_identity_phi_sites_and_records_entries() {
        let (function, view, load, store) = packed_double_cache_function(false, false);
        let plan = function.plan_packed_double_view_caches(&view);
        assert_eq!(plan.caches.len(), 1);
        let cache = &plan.caches[0];
        assert_eq!(cache.id.index(), 0);
        assert_eq!(cache.loop_header, 1);
        assert_eq!(cache.receiver_root, NumericValue(0));
        assert_eq!(cache.entry_edges, BTreeSet::from([(0, 0)]));
        assert!(cache.access.is_packed_double_array());
        assert_eq!(plan.cache_for(load), Some(cache.id));
        assert_eq!(plan.cache_for(store), Some(cache.id));
    }

    #[test]
    fn built_hir_plans_one_cache_for_a_real_packed_double_loop() {
        let view = packed_double_loop_view();
        let function = NumericFunction::build(&view).expect("packed-double loop HIR");
        let plan = function.plan_packed_double_view_caches(&view);
        assert_eq!(plan.caches.len(), 1);
        let cache = &plan.caches[0];
        assert!(!cache.entry_edges.is_empty());
        assert!(matches!(
            function.nodes[cache.receiver_root.0],
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Tagged
            }
        ));
        let sites = function
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match node {
                NumericNode::ElementLoad {
                    byte_pc: 32,
                    access: NumericElementAccess::PackedDouble,
                    ..
                }
                | NumericNode::ElementStore {
                    byte_pc: 40,
                    access: NumericElementAccess::PackedDouble,
                    ..
                } => Some(NumericValue(index)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(sites.len(), 2);
        assert!(
            sites
                .into_iter()
                .all(|site| plan.cache_for(site) == Some(cache.id))
        );
    }

    #[test]
    fn packed_double_view_cache_rejects_varying_receivers_and_reentrant_loops() {
        let (varying, view, _, _) = packed_double_cache_function(true, false);
        assert!(
            varying
                .plan_packed_double_view_caches(&view)
                .caches
                .is_empty(),
            "a changing header phi must not retain a raw view"
        );

        let (unsafe_function, view, _, _) = packed_double_cache_function(false, true);
        assert!(
            unsafe_function
                .plan_packed_double_view_caches(&view)
                .caches
                .is_empty(),
            "an allocating loop must clear rather than retain raw addresses"
        );
    }

    #[test]
    fn packed_double_view_cache_plans_only_innermost_natural_loops() {
        let block = |predecessors: Vec<usize>, successors: Vec<usize>| NumericBlock {
            logical_pc: 0,
            osr_entry_allowed: true,
            predecessors,
            successor_arguments: vec![Vec::new(); successors.len()],
            successors,
            parameters: Vec::new(),
            parameter_registers: Vec::new(),
            nodes: Vec::new(),
            terminator: NumericTerminator::Jump,
        };
        let blocks = vec![
            block(Vec::new(), vec![1]),
            block(vec![0, 4], vec![2, 5]),
            block(vec![1, 3], vec![3, 4]),
            block(vec![2], vec![2]),
            block(vec![2], vec![1]),
            block(vec![1], Vec::new()),
        ];
        let loops = innermost_reducible_natural_loops(&blocks).expect("reducible CFG");
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].header, 2);
        assert_eq!(loops[0].blocks, BTreeSet::from([2, 3]));

        let self_loop = vec![
            block(Vec::new(), vec![1]),
            block(vec![0, 1], vec![1, 2]),
            block(vec![1], Vec::new()),
        ];
        let loops = innermost_reducible_natural_loops(&self_loop).expect("self loop CFG");
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].header, 1);
        assert_eq!(loops[0].blocks, BTreeSet::from([1]));
    }

    #[test]
    fn int32_feedback_does_not_narrow_an_existing_number_ssa_value() {
        for op in [
            Op::Increment,
            Op::AddImm,
            Op::SubImm,
            Op::LessThanImm,
            Op::EqualImm,
            Op::NotEqualImm,
        ] {
            let mut view = JitCompileSnapshot::without_feedback(
                119,
                0,
                5,
                vec![
                    JitTestInstruction::new(
                        Op::LoadUpvalue,
                        0,
                        0,
                        vec![Operand::Register(0), Operand::Imm32(0)],
                    ),
                    JitTestInstruction::new(
                        Op::ToPrimitive,
                        1,
                        8,
                        vec![
                            Operand::Register(1),
                            Operand::Register(0),
                            Operand::ConstIndex(13),
                        ],
                    ),
                    JitTestInstruction::new(
                        Op::ToNumeric,
                        2,
                        16,
                        vec![Operand::Register(2), Operand::Register(1)],
                    ),
                    JitTestInstruction::new(
                        op,
                        3,
                        24,
                        vec![
                            Operand::Register(3),
                            Operand::Register(2),
                            Operand::Imm32(1),
                        ],
                    ),
                    JitTestInstruction::new(Op::ReturnValue, 4, 32, vec![Operand::Register(3)]),
                ],
            );
            view.cage_base = 0x1000;
            view.seed_arith_feedback_for_test(3, ArithFeedback::from_bits(ARITH_INT32));

            let hir = NumericFunction::build(&view).expect("refined Number HIR");
            let number = hir
                .nodes
                .iter()
                .position(|node| matches!(node, NumericNode::TaggedToNumber(..)))
                .map(NumericValue)
                .expect("ToNumeric Number value");
            assert_eq!(hir.nodes[number.0].value_type(), NumericType::Number);
            assert!(hir.nodes.iter().any(|node| match (op, node) {
                (Op::Increment | Op::AddImm, NumericNode::Add(left, _))
                | (Op::SubImm, NumericNode::Sub(left, _))
                | (Op::LessThanImm, NumericNode::LessThan(left, _))
                | (Op::EqualImm, NumericNode::Equal(left, _))
                | (Op::NotEqualImm, NumericNode::NotEqual(left, _)) => *left == number,
                _ => false,
            }));
            assert!(hir.nodes.iter().all(|node| !matches!(
                node,
                NumericNode::IntegerAddImmediate(..)
                    | NumericNode::IntegerSubImmediate(..)
                    | NumericNode::IntegerLessThanImmediate(..)
                    | NumericNode::IntegerEqualImmediate(..)
                    | NumericNode::IntegerNotEqualImmediate(..)
            )));
        }
    }

    #[test]
    fn mixed_numeric_loop_phi_retries_as_number_but_constructor_transitions_reject_it() {
        let mut view = mixed_numeric_loop_view();
        view.seed_arith_feedback_for_test(1, ArithFeedback::from_bits(ARITH_INT32));
        let hir = NumericFunction::build(&view).expect("mixed numeric loop HIR");
        let header = hir
            .blocks
            .iter()
            .find(|block| block.logical_pc == 1)
            .expect("loop header");
        let index = header
            .parameter_registers
            .iter()
            .position(|&register| register == 0)
            .expect("loop-carried local");
        let parameter = header.parameters[index];
        assert_eq!(
            hir.nodes[parameter.0],
            NumericNode::BlockParameter(NumericType::Number)
        );
        let incoming_types = header
            .predecessors
            .iter()
            .map(|&predecessor| {
                let edge = hir.blocks[predecessor]
                    .successors
                    .iter()
                    .position(|&successor| successor == 1)
                    .expect("incoming header edge");
                value_type(
                    &hir.nodes,
                    hir.blocks[predecessor].successor_arguments[edge][index],
                )
                .expect("incoming value type")
            })
            .collect::<Vec<_>>();
        assert!(incoming_types.contains(&NumericType::Int32));
        assert!(incoming_types.contains(&NumericType::Number));
        assert!(hir.nodes.iter().any(|node| {
            matches!(node, NumericNode::LessThan(left, right)
                if *left == parameter
                    && matches!(hir.nodes[right.0], NumericNode::Constant(3.0)))
        }));
        assert!(hir.nodes.iter().all(|node| {
            !matches!(node, NumericNode::IntegerLessThanImmediate(source, 3) if *source == parameter)
        }));

        let mut constructor = view;
        constructor.constructor_field_transitions.insert(
            999,
            JitConstructorFieldTransition {
                from_shape: 1,
                to_shape: 2,
                prototype_shapes: vec![3],
                slot: 0,
            },
        );
        assert!(
            NumericFunction::build(&constructor).is_err(),
            "mixed representations must keep constructor transitions on the Template baseline"
        );
    }

    #[test]
    fn array_construct_inference_accesses_and_exception_liveness_are_exact() {
        let zero = array_construct_view(0);
        let zero_code = zero.code_block.as_ref();
        let mut zero_origins = vec![1, 2, 4];
        let mut zero_int32 = 0;
        let mut zero_number = 0;
        infer_instruction_parameters(
            &zero.instructions[0],
            zero_code,
            &mut zero_origins,
            &mut zero_int32,
            &mut zero_number,
        )
        .expect("zero-argument inference");
        assert_eq!(zero_origins, [1, 2, 0]);
        assert_eq!(zero_int32, 0);
        assert_eq!(
            instruction_accesses(&zero.instructions[0], zero_code),
            Some((Vec::new(), vec![2]))
        );

        let one = array_construct_view(1);
        let one_code = one.code_block.as_ref();
        let mut one_origins = vec![1, 2, 4];
        let mut one_int32 = 0;
        let mut one_number = 0;
        infer_instruction_parameters(
            &one.instructions[0],
            one_code,
            &mut one_origins,
            &mut one_int32,
            &mut one_number,
        )
        .expect("one-argument inference");
        assert_eq!(one_origins, [1, 2, 0]);
        assert_eq!(one_int32, 1);
        assert_eq!(one_number, 0);
        assert_eq!(
            instruction_accesses(&one.instructions[0], one_code),
            Some((vec![0], vec![2]))
        );

        let mut live = vec![false; 6];
        let mut catch_live = vec![false; 6];
        catch_live[4] = true;
        catch_live[5] = true;
        let semantics = classify_snapshot(&one).expect("ArrayConstruct semantics");
        transfer_instruction_liveness(
            &one.instructions[0],
            one_code,
            Some(InstructionExceptionHandler {
                block: 0,
                exception_register: 4,
            }),
            &[catch_live],
            semantics[0],
            &mut live,
        )
        .expect("snapshot-aware ArrayConstruct liveness");
        assert!(!semantics[0].has_implicit_exception_side_exit(Op::ArrayConstruct));
        assert!(live[0], "length is an ordinary read");
        assert!(!live[2], "destination is killed before the operation");
        assert!(!live[4], "the catch supplies its exception register");
        assert!(!live[5], "the allocation path cannot throw semantically");

        let two = array_construct_view(2);
        let two_code = two.code_block.as_ref();
        assert_eq!(
            instruction_accesses(&two.instructions[0], two_code),
            Some((vec![0, 1], vec![2]))
        );
        let mut origins = [1, 2, 4];
        let mut int32_parameters = 0;
        let mut number_parameters = 0;
        infer_instruction_parameters(
            &two.instructions[0],
            two_code,
            &mut origins,
            &mut int32_parameters,
            &mut number_parameters,
        )
        .expect("multi-value ArrayConstruct uses the generic schema transfer");
        assert_eq!(origins, [1, 2, 0]);
        assert_eq!(int32_parameters, 0);
        assert_eq!(number_parameters, 0);
    }

    #[test]
    fn ordinary_properties_build_without_settled_metadata_and_keep_exact_states() {
        let view = property_view();
        assert!(view.property_loads.is_empty());
        assert!(view.property_stores.is_empty());

        let hir = NumericFunction::build(&view).expect("property HIR without settled metadata");
        let receiver = hir
            .nodes
            .iter()
            .position(|node| *node == NumericNode::IntegerConstant(7))
            .map(NumericValue)
            .expect("Int32 receiver");
        let stored = hir
            .nodes
            .iter()
            .position(|node| *node == NumericNode::BooleanConstant(true))
            .map(NumericValue)
            .expect("Boolean stored value");
        let load = hir
            .nodes
            .iter()
            .position(|node| {
                *node
                    == NumericNode::PropertyLoad {
                        receiver,
                        byte_pc: 8,
                        exotic_length: false,
                    }
            })
            .map(NumericValue)
            .expect("ordinary property load");
        let store = hir
            .nodes
            .iter()
            .position(|node| {
                *node
                    == NumericNode::PropertyStore {
                        receiver,
                        value: stored,
                        byte_pc: 24,
                    }
            })
            .map(NumericValue)
            .expect("ordinary property store");
        assert_eq!(hir.nodes[load.0].value_type(), NumericType::Tagged);

        let load_state = hir
            .frame_states
            .iter()
            .find(|state| state.point == NumericFramePoint::Node(load))
            .expect("exact pre-load state");
        assert_eq!(load_state.byte_pc, 8);
        assert_eq!(load_state.slots[0], NumericFrameSlot::Value(receiver));
        assert_eq!(load_state.slots[2], NumericFrameSlot::Undefined);

        let store_state = hir
            .frame_states
            .iter()
            .find(|state| state.point == NumericFramePoint::Node(store))
            .expect("exact pre-store state");
        assert_eq!(store_state.byte_pc, 24);
        assert_eq!(store_state.slots[0], NumericFrameSlot::Value(receiver));
        assert_eq!(store_state.slots[1], NumericFrameSlot::Value(stored));
        assert_eq!(store_state.slots[2], NumericFrameSlot::Value(load));
        assert_eq!(store_state.slots[3], NumericFrameSlot::Undefined);
    }

    #[test]
    fn named_length_property_load_retains_its_exotic_marker() {
        let mut view = property_view();
        view.instructions[1].load_array_length = true;

        let hir = NumericFunction::build(&view).expect("named length property HIR");
        assert!(hir.nodes.iter().any(|node| matches!(
            node,
            NumericNode::PropertyLoad {
                byte_pc: 8,
                exotic_length: true,
                ..
            }
        )));
    }

    #[test]
    fn property_inference_and_accesses_validate_constants_and_register_roles() {
        let view = property_view();
        let code = view.code_block.as_ref();
        let mut origins = vec![1, 2, 4, 8];
        let mut int32_parameters = 0;
        let mut number_parameters = 0;

        infer_instruction_parameters(
            &view.instructions[1],
            code,
            &mut origins,
            &mut int32_parameters,
            &mut number_parameters,
        )
        .expect("property load inference");
        assert_eq!(origins, [1, 2, 0, 8]);
        assert_eq!(view.instructions[1].const_index(code, 2), Some(9));
        assert_eq!(
            instruction_accesses(&view.instructions[1], code),
            Some((vec![0], vec![2]))
        );

        infer_instruction_parameters(
            &view.instructions[3],
            code,
            &mut origins,
            &mut int32_parameters,
            &mut number_parameters,
        )
        .expect("property store inference");
        assert_eq!(origins, [1, 2, 0, 0]);
        assert_eq!(view.instructions[3].const_index(code, 1), Some(10));
        assert_eq!(
            instruction_accesses(&view.instructions[3], code),
            Some((vec![0, 1], vec![3]))
        );
        assert_eq!(int32_parameters, 0);
        assert_eq!(number_parameters, 0);
    }

    #[test]
    fn property_store_kills_its_accessor_scratch() {
        let view = JitCompileSnapshot::without_feedback(
            104,
            2,
            3,
            vec![
                JitTestInstruction::new(
                    Op::StoreProperty,
                    0,
                    0,
                    vec![
                        Operand::Register(0),
                        Operand::ConstIndex(0),
                        Operand::Register(1),
                        Operand::Register(2),
                    ],
                ),
                JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(2)]),
            ],
        );
        assert!(
            NumericFunction::build(&view).is_err(),
            "the opaque setter scratch must not become a reusable HIR value"
        );
    }

    #[test]
    fn load_property_implicit_exception_exit_keeps_catch_only_values_live() {
        let view = property_catch_liveness_view();
        let semantics = classify_snapshot(&view).expect("property semantics");
        let raw_blocks = build_raw_blocks(&view, &semantics).expect("exception-aware raw blocks");
        let handlers = build_instruction_exception_handlers(&view, &raw_blocks, &semantics)
            .expect("per-instruction catch handlers");
        let live_in = build_liveness(&view, &raw_blocks, &handlers, &semantics, 8)
            .expect("exception-aware block liveness");
        let property_block = raw_blocks
            .iter()
            .position(|block| block.start == 5)
            .expect("property block after normal boundary");
        let property_exception_edge = raw_blocks[property_block]
            .exceptional_edge
            .expect("schema may-throw property terminates with a catch edge");
        let handler = raw_blocks
            .iter()
            .position(|block| block.start == 11)
            .expect("catch block");
        assert_eq!(
            raw_blocks[property_block].successors[property_exception_edge],
            handler
        );
        assert!(
            live_in[property_block][5],
            "LoadProperty may throw to the catch that reads the redefined value"
        );

        assert!(
            NumericFunction::build(&view).is_err(),
            "a runtime-backed property operation inside a local catch stays on the Template baseline"
        );
    }

    #[test]
    fn catch_only_definition_remains_live_at_rejected_committed_element() {
        let view = catch_liveness_view();
        let semantics = classify_snapshot(&view).expect("element semantics");
        let raw_blocks = build_raw_blocks(&view, &semantics).expect("exception-aware raw blocks");
        let handlers = build_instruction_exception_handlers(&view, &raw_blocks, &semantics)
            .expect("per-instruction catch handlers");
        let live_in = build_liveness(&view, &raw_blocks, &handlers, &semantics, 9)
            .expect("exception-aware block liveness");
        let after_call = raw_blocks
            .iter()
            .position(|block| block.start == 3)
            .expect("post-call definition block");
        let element_block = raw_blocks
            .iter()
            .position(|block| block.start == 5)
            .expect("element block after normal boundary");
        assert!(
            !live_in[after_call][5],
            "the definition must kill the call-edge value at block entry"
        );
        assert!(
            live_in[element_block][5],
            "the later catch side exit must retain the redefined value across the boundary"
        );

        assert!(
            NumericFunction::build(&view).is_err(),
            "a prepared tagged element miss cannot enter a local catch before committed-throw landing exists"
        );
    }
}
