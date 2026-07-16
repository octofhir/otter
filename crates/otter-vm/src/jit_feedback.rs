//! Dense tier-neutral instruction feedback owned by executable code.
//!
//! The interpreter records observed operand/value representations at numeric
//! bytecode sites while a hot function is still warming up. Cells live in the
//! owning [`crate::CodeBlock`] at the canonical instruction index, so recording
//! and compilation never hash `(function_id, pc)` pairs or copy feedback into a
//! parallel interpreter-owned map.
//!
//! # Contents
//! - [`ArithFeedback`] — decoded arithmetic representation bits.
//! - [`FeedbackVector`] — cells and their single monotonic transition epoch.
//! - [`InstructionFeedback`] — one dense atomic cell per CodeBlock instruction.
//! - [`InstructionFeedbackRecorder`] — a vector-bound recording view that
//!   advances the owning vector epoch on material transitions.
//! - Fixed-layout call and property-summary slots selected by opcode at
//!   CodeBlock construction; method sites carry only a directory marker.
//!
//! # Invariants
//! - **Monotonic.** Bits are only ever set, never cleared. A site that has ever
//!   observed a non-numeric operand can therefore never be mis-speculated as
//!   numeric: the optimizing tier's "numeric only" test fails permanently once
//!   a string / bigint / object operand is seen.
//! - **Advisory.** A site that was never recorded reads as empty
//!   ([`ArithFeedback::is_empty`]); the optimizing tier treats that as unknown
//!   and lowers it generically. Dropping or losing feedback is always sound —
//!   only less fast.
//! - Recording happens only while a JIT hook is installed; interpreter-only
//!   execution never touches these cells.
//! - The vector's feedback epoch advances once per material state transition,
//!   never for an already-recorded observation.
//! - The isolate's VM thread is the sole writer. Arithmetic/element bits are
//!   advisory monotonic atomics. Multiword property and bounded-call records
//!   publish coherent reader snapshots with a per-slot sequence counter.
//! - Fixed slots contain atomics and stable numeric ids only; no `Value`, GC
//!   handle, upvalue, closure, or `this` crosses the CodeBlock Send/Sync
//!   boundary. Method distributions remain isolate-owned behind
//!   [`crate::interp::FeedbackDirectory`].
//!
//! # See also
//! - [`crate::CodeBlock`] — owner of the live [`FeedbackVector`].

use std::hint::spin_loop;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering, fence};

use otter_bytecode::Op;
use smallvec::SmallVec;

use crate::cache_ir::CacheStub;
use crate::jit::JitElementLoadKind;
use crate::property_ic::{PropertyIcEntry, PropertyIcKind};
use crate::{CallTargetFeedback, Value};

/// At least one operand was an `int32` fast-path number.
pub const ARITH_INT32: u8 = 1 << 0;
/// At least one operand was a non-int32 (double) number, including
/// NaN / ±Infinity.
pub const ARITH_FLOAT64: u8 = 1 << 1;
/// At least one operand was a string (the `+` concat path, or a relational
/// string comparison).
pub const ARITH_STRING: u8 = 1 << 2;
/// At least one operand was a BigInt.
pub const ARITH_BIGINT: u8 = 1 << 3;
/// At least one operand was none of the above: boolean, null, undefined,
/// symbol, or object (requiring a full `ToPrimitive` / `ToNumeric`).
pub const ARITH_OTHER: u8 = 1 << 4;

/// Non-numeric observation bits. A site with any of these set can never be
/// speculated as a pure numeric operation.
const NON_NUMERIC: u8 = ARITH_STRING | ARITH_BIGINT | ARITH_OTHER;
const ARITH_WIDEN_FLOAT: u8 = 1 << 7;

const ELEMENT_UNSEEN: u8 = 0;
const ELEMENT_FLOAT64: u8 = 1;
const ELEMENT_INT32: u8 = 2;
const ELEMENT_GENERIC: u8 = 3;
const ELEMENT_MASK: u8 = 0b0000_0011;

const CALL_UNSEEN: u8 = 0;
const CALL_MONO: u8 = 1;
const CALL_POLY: u8 = 2;
const CALL_SHIFT: u8 = 2;
const CALL_MASK: u8 = 0b0000_1100;
const BRANCH_TAKEN_SEEN: u8 = 1 << 4;
const BRANCH_NOT_TAKEN_SEEN: u8 = 1 << 5;

/// Material transition made while recording an ordinary bytecode call target.
///
/// The existing baseline invalidation policy only reacts to
/// [`Self::BecameMonomorphic`]. Keeping that decision distinct from the broader
/// state-change signal lets the feedback epoch also observe mono-to-poly without
/// changing baseline recompilation behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallTargetTransition {
    /// The observation was already represented by the dense cell.
    Unchanged,
    /// The previously unseen site recorded its first bytecode callee.
    BecameMonomorphic,
    /// A monomorphic site observed a different bytecode callee.
    BecamePolymorphic,
}

impl CallTargetTransition {
    /// Whether the dense call-target state changed.
    #[must_use]
    pub(crate) const fn state_changed(self) -> bool {
        !matches!(self, Self::Unchanged)
    }

    /// Preserve the baseline's existing first-target invalidation decision.
    #[must_use]
    pub(crate) const fn evict_for_reopt(self) -> bool {
        matches!(self, Self::BecameMonomorphic)
    }
}

/// Maximum distinct bytecode callees retained at one ordinary-call site.
pub(crate) const MAX_CALL_TARGETS: usize = 8;

/// One observed bytecode callee and its saturating execution count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallTargetCount {
    pub(crate) fid: u32,
    pub(crate) hits: u32,
}

/// Immutable snapshot of the bounded target population for one `Op::Call`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallSiteDistribution {
    Mono(CallTargetCount),
    Poly(Box<SmallVec<[CallTargetCount; MAX_CALL_TARGETS]>>),
    Megamorphic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DistributionTransition {
    Unchanged,
    /// The dense unseen/mono/poly state already accounts for this transition.
    MirroredByDenseCell,
    /// The bounded target set gained information beyond dense mono/poly state.
    Extended,
}

/// Stable tier-facing summary of an isolate-owned property IC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PropertyFeedbackState {
    #[default]
    Empty,
    MonomorphicOwnData {
        shape_id: crate::object::ShapeId,
        slot: u16,
    },
    Polymorphic,
    Megamorphic,
}

const PROPERTY_EMPTY: u8 = 0;
const PROPERTY_MONOMORPHIC_OWN_DATA: u8 = 1;
const PROPERTY_POLYMORPHIC: u8 = 2;
const PROPERTY_MEGAMORPHIC: u8 = 3;

/// Fixed-size atomic property summary. The isolate VM thread is the sole
/// writer; readers take a coherent snapshot with `sequence` as a seqlock.
#[derive(Debug)]
struct AtomicPropertyFeedback {
    sequence: AtomicU32,
    kind: PropertyIcKind,
    state: AtomicU8,
    shape_id: AtomicU64,
    slot: AtomicU16,
}

impl AtomicPropertyFeedback {
    fn new(kind: PropertyIcKind) -> Self {
        Self {
            sequence: AtomicU32::new(0),
            kind,
            state: AtomicU8::new(PROPERTY_EMPTY),
            shape_id: AtomicU64::new(0),
            slot: AtomicU16::new(0),
        }
    }

    fn publish(&self, state: PropertyFeedbackState) {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(sequence & 1, 0, "property feedback has one writer");

        let (tag, shape_id, slot) = match state {
            PropertyFeedbackState::Empty => (PROPERTY_EMPTY, 0, 0),
            PropertyFeedbackState::MonomorphicOwnData { shape_id, slot } => {
                (PROPERTY_MONOMORPHIC_OWN_DATA, shape_id.raw(), slot)
            }
            PropertyFeedbackState::Polymorphic => (PROPERTY_POLYMORPHIC, 0, 0),
            PropertyFeedbackState::Megamorphic => (PROPERTY_MEGAMORPHIC, 0, 0),
        };
        self.shape_id.store(shape_id, Ordering::Relaxed);
        self.slot.store(slot, Ordering::Relaxed);
        self.state.store(tag, Ordering::Relaxed);
        let sequence = self.sequence.fetch_add(1, Ordering::Release);
        debug_assert_eq!(sequence & 1, 1, "property feedback publication must close");
    }

    fn snapshot(&self) -> PropertyFeedbackState {
        loop {
            let start = self.sequence.load(Ordering::Acquire);
            if start & 1 != 0 {
                spin_loop();
                continue;
            }
            let state = self.state.load(Ordering::Relaxed);
            let shape_id = self.shape_id.load(Ordering::Relaxed);
            let slot = self.slot.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            let end = self.sequence.load(Ordering::Relaxed);
            if start != end {
                spin_loop();
                continue;
            }
            return match state {
                PROPERTY_EMPTY => PropertyFeedbackState::Empty,
                PROPERTY_MONOMORPHIC_OWN_DATA => PropertyFeedbackState::MonomorphicOwnData {
                    shape_id: crate::object::ShapeId::from_raw(shape_id),
                    slot,
                },
                PROPERTY_POLYMORPHIC => PropertyFeedbackState::Polymorphic,
                PROPERTY_MEGAMORPHIC => PropertyFeedbackState::Megamorphic,
                _ => unreachable!("invalid atomic property feedback state"),
            };
        }
    }
}

impl Clone for AtomicPropertyFeedback {
    fn clone(&self) -> Self {
        let cloned = Self::new(self.kind);
        cloned.publish(self.snapshot());
        cloned
    }
}

const CALL_DISTRIBUTION_EMPTY: u8 = 0;
const CALL_DISTRIBUTION_MONO: u8 = 1;
const CALL_DISTRIBUTION_POLY: u8 = 2;
const CALL_DISTRIBUTION_MEGAMORPHIC: u8 = 3;

const fn pack_call_target(target: CallTargetCount) -> u64 {
    (target.fid as u64) << 32 | target.hits as u64
}

const fn unpack_call_target(packed: u64) -> CallTargetCount {
    CallTargetCount {
        fid: (packed >> 32) as u32,
        hits: packed as u32,
    }
}

/// Fixed-capacity ordinary-call distribution. Target records are packed as
/// `(function id, hits)` in one atomic word and never allocate after slot
/// construction.
#[derive(Debug)]
struct AtomicCallFeedback {
    sequence: AtomicU32,
    state: AtomicU8,
    count: AtomicU8,
    targets: [AtomicU64; MAX_CALL_TARGETS],
}

impl Default for AtomicCallFeedback {
    fn default() -> Self {
        Self {
            sequence: AtomicU32::new(0),
            state: AtomicU8::new(CALL_DISTRIBUTION_EMPTY),
            count: AtomicU8::new(0),
            targets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl AtomicCallFeedback {
    fn record(&self, callee_fid: u32) -> DistributionTransition {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(sequence & 1, 0, "call feedback has one writer");

        let state = self.state.load(Ordering::Relaxed);
        let count = usize::from(self.count.load(Ordering::Relaxed));
        let mut transition = DistributionTransition::Unchanged;
        match state {
            CALL_DISTRIBUTION_EMPTY => {
                self.targets[0].store(
                    pack_call_target(CallTargetCount {
                        fid: callee_fid,
                        hits: 1,
                    }),
                    Ordering::Relaxed,
                );
                self.count.store(1, Ordering::Relaxed);
                self.state.store(CALL_DISTRIBUTION_MONO, Ordering::Relaxed);
                transition = DistributionTransition::MirroredByDenseCell;
            }
            CALL_DISTRIBUTION_MONO | CALL_DISTRIBUTION_POLY => {
                let existing = self.targets[..count].iter().position(|target| {
                    unpack_call_target(target.load(Ordering::Relaxed)).fid == callee_fid
                });
                if let Some(index) = existing {
                    let target = unpack_call_target(self.targets[index].load(Ordering::Relaxed));
                    self.targets[index].store(
                        pack_call_target(CallTargetCount {
                            hits: target.hits.saturating_add(1),
                            ..target
                        }),
                        Ordering::Relaxed,
                    );
                } else if count < MAX_CALL_TARGETS {
                    self.targets[count].store(
                        pack_call_target(CallTargetCount {
                            fid: callee_fid,
                            hits: 1,
                        }),
                        Ordering::Relaxed,
                    );
                    self.count.store((count + 1) as u8, Ordering::Relaxed);
                    self.state.store(CALL_DISTRIBUTION_POLY, Ordering::Relaxed);
                    transition = if count == 1 {
                        DistributionTransition::MirroredByDenseCell
                    } else {
                        DistributionTransition::Extended
                    };
                } else {
                    self.state
                        .store(CALL_DISTRIBUTION_MEGAMORPHIC, Ordering::Relaxed);
                    transition = DistributionTransition::Extended;
                }
            }
            CALL_DISTRIBUTION_MEGAMORPHIC => {}
            _ => unreachable!("invalid atomic call feedback state"),
        }

        let sequence = self.sequence.fetch_add(1, Ordering::Release);
        debug_assert_eq!(sequence & 1, 1, "call feedback publication must close");
        transition
    }

    fn snapshot(&self) -> Option<CallSiteDistribution> {
        loop {
            let start = self.sequence.load(Ordering::Acquire);
            if start & 1 != 0 {
                spin_loop();
                continue;
            }
            let state = self.state.load(Ordering::Relaxed);
            let count = usize::from(self.count.load(Ordering::Relaxed));
            let mut targets: SmallVec<[CallTargetCount; MAX_CALL_TARGETS]> = SmallVec::new();
            for target in self.targets.iter().take(count.min(MAX_CALL_TARGETS)) {
                targets.push(unpack_call_target(target.load(Ordering::Relaxed)));
            }
            fence(Ordering::Acquire);
            let end = self.sequence.load(Ordering::Relaxed);
            if start != end {
                spin_loop();
                continue;
            }
            return match state {
                CALL_DISTRIBUTION_EMPTY => None,
                CALL_DISTRIBUTION_MONO => targets.first().copied().map(CallSiteDistribution::Mono),
                CALL_DISTRIBUTION_POLY => Some(CallSiteDistribution::Poly(Box::new(targets))),
                CALL_DISTRIBUTION_MEGAMORPHIC => Some(CallSiteDistribution::Megamorphic),
                _ => unreachable!("invalid atomic call feedback state"),
            };
        }
    }
}

impl Clone for AtomicCallFeedback {
    fn clone(&self) -> Self {
        let cloned = Self::default();
        let Some(snapshot) = self.snapshot() else {
            return cloned;
        };
        match snapshot {
            CallSiteDistribution::Mono(target) => {
                cloned.targets[0].store(pack_call_target(target), Ordering::Relaxed);
                cloned.count.store(1, Ordering::Relaxed);
                cloned
                    .state
                    .store(CALL_DISTRIBUTION_MONO, Ordering::Relaxed);
            }
            CallSiteDistribution::Poly(targets) => {
                for (index, target) in targets.iter().copied().enumerate() {
                    cloned.targets[index].store(pack_call_target(target), Ordering::Relaxed);
                }
                cloned.count.store(targets.len() as u8, Ordering::Relaxed);
                cloned
                    .state
                    .store(CALL_DISTRIBUTION_POLY, Ordering::Relaxed);
            }
            CallSiteDistribution::Megamorphic => {
                cloned
                    .state
                    .store(CALL_DISTRIBUTION_MEGAMORPHIC, Ordering::Relaxed);
            }
        }
        cloned
    }
}

/// Opcode-selected fixed-layout feedback storage. Boxes are allocated once
/// while the CodeBlock is built; their atomic payloads never resize or retain
/// GC-managed values.
#[derive(Debug, Clone)]
enum TypedFeedbackSlot {
    None,
    Property(Box<AtomicPropertyFeedback>),
    Method,
    Call(Box<AtomicCallFeedback>),
}

impl TypedFeedbackSlot {
    fn for_op(op: Op) -> Self {
        match op {
            Op::LoadProperty => {
                Self::Property(Box::new(AtomicPropertyFeedback::new(PropertyIcKind::Load)))
            }
            Op::StoreProperty => {
                Self::Property(Box::new(AtomicPropertyFeedback::new(PropertyIcKind::Store)))
            }
            Op::HasProperty => {
                Self::Property(Box::new(AtomicPropertyFeedback::new(PropertyIcKind::Has)))
            }
            Op::CallMethodValue => Self::Method,
            Op::Call => Self::Call(Box::default()),
            _ => Self::None,
        }
    }
}

/// Typed view over one `LoadProperty` / `StoreProperty` / `HasProperty` cache.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PropertyFeedbackSlot<'a> {
    feedback: &'a AtomicPropertyFeedback,
}

impl PropertyFeedbackSlot<'_> {
    #[must_use]
    pub(crate) fn state(self) -> PropertyFeedbackState {
        self.feedback.snapshot()
    }

    /// Publish a stable snapshot of the isolate-owned runtime IC state.
    pub(crate) fn publish(self, entry: &PropertyIcEntry<CacheStub>) {
        let state = match entry {
            PropertyIcEntry::Empty => PropertyFeedbackState::Empty,
            PropertyIcEntry::Megamorphic => PropertyFeedbackState::Megamorphic,
            PropertyIcEntry::Polymorphic { entries, .. } => match entries.as_slice() {
                [stub] => {
                    let hit = match self.feedback.kind {
                        PropertyIcKind::Load => stub.own_data_hit(),
                        PropertyIcKind::Store => stub.store_own_data_hit(),
                        PropertyIcKind::Has => None,
                    };
                    hit.map_or(PropertyFeedbackState::Polymorphic, |hit| {
                        PropertyFeedbackState::MonomorphicOwnData {
                            shape_id: hit.shape_id,
                            slot: hit.slot,
                        }
                    })
                }
                _ => PropertyFeedbackState::Polymorphic,
            },
        };
        self.feedback.publish(state);
    }
}

/// Typed view over the bounded target distribution for one ordinary call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallFeedbackSlot<'a> {
    feedback: &'a AtomicCallFeedback,
}

impl CallFeedbackSlot<'_> {
    fn record(self, callee_fid: u32) -> DistributionTransition {
        self.feedback.record(callee_fid)
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn distribution(self) -> Option<CallSiteDistribution> {
        self.feedback.snapshot()
    }
}

/// OR-accumulated representation feedback for one numeric-specialized bytecode
/// site.
///
/// The interpreter folds both operands of every observed execution into the
/// same cell, so the bitset summarises *every representation ever seen at the
/// site*, across executions and across the two operand positions. The
/// optimizing tier reads it to decide whether the site is safe to lower as a
/// speculative `Int32` or `Float64` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ArithFeedback(u8);

impl ArithFeedback {
    /// Construct a feedback cell directly from raw observation bits. Used when
    /// baking the interpreter cell into the borrow-free compile snapshot.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// Raw observation bits, for the baked compile snapshot.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// `true` when no operand representation has ever been recorded.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Fold both operands of one observed execution into the cell.
    pub fn record(&mut self, lhs: Value, rhs: Value) {
        self.0 |= Self::classify(lhs) | Self::classify(rhs);
    }

    /// Representation bit for one operand value.
    fn classify(value: Value) -> u8 {
        if value.is_int32() {
            ARITH_INT32
        } else if value.is_number() {
            ARITH_FLOAT64
        } else if value.is_string() {
            ARITH_STRING
        } else if value.is_big_int() {
            ARITH_BIGINT
        } else {
            ARITH_OTHER
        }
    }

    /// `true` when every operand ever seen was a number (int32 or double) and
    /// the site was observed at least once — the precondition for speculating a
    /// `Float64` lowering with an "is number" guard.
    #[must_use]
    pub const fn is_numeric_only(self) -> bool {
        self.0 != 0 && (self.0 & NON_NUMERIC) == 0
    }

    /// `true` when every operand ever seen was an `int32` — the precondition for
    /// speculating an unboxed `Int32` lowering with an "is int32" guard.
    #[must_use]
    pub const fn is_int32_only(self) -> bool {
        self.0 == ARITH_INT32
    }

    /// `true` when this site has never executed. Optimized code may treat it
    /// as unreachable-by-feedback and deoptimize unconditionally if it is ever
    /// reached: the interpreter then records real feedback and the next
    /// compile sees it.
    #[must_use]
    pub const fn is_unseen(self) -> bool {
        self.0 == 0
    }
}

/// Dense feedback owned by one canonical CodeBlock instruction.
#[derive(Debug, Default)]
pub struct InstructionFeedback {
    arith: AtomicU8,
    states: AtomicU8,
    branch_taken: AtomicU8,
    branch_total: AtomicU8,
    call_target: AtomicU32,
}

impl Clone for InstructionFeedback {
    fn clone(&self) -> Self {
        Self {
            arith: AtomicU8::new(self.arith.load(Ordering::Relaxed)),
            states: AtomicU8::new(self.states.load(Ordering::Acquire)),
            branch_taken: AtomicU8::new(self.branch_taken.load(Ordering::Relaxed)),
            branch_total: AtomicU8::new(self.branch_total.load(Ordering::Relaxed)),
            call_target: AtomicU32::new(self.call_target.load(Ordering::Relaxed)),
        }
    }
}

impl InstructionFeedback {
    /// Record one conditional-branch outcome in this instruction's dense cell.
    /// Returns `true` only for the first observation of each direction; compact
    /// hit counters continue saturating independently on every observation.
    pub fn record_branch(&self, taken: bool) -> bool {
        let saturating_increment = |value: u8| Some(value.saturating_add(1));
        let _ = self.branch_total.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            saturating_increment,
        );
        if taken {
            let _ = self.branch_taken.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                saturating_increment,
            );
        }
        let seen = if taken {
            BRANCH_TAKEN_SEEN
        } else {
            BRANCH_NOT_TAKEN_SEEN
        };
        self.states.fetch_or(seen, Ordering::Relaxed) & seen == 0
    }

    /// `(taken, total)` conditional-branch observations.
    #[must_use]
    pub fn branch_counts(&self) -> (u8, u8) {
        (
            self.branch_taken.load(Ordering::Relaxed),
            self.branch_total.load(Ordering::Relaxed),
        )
    }

    /// Fold one observed arithmetic operand pair into this instruction cell.
    /// Returns `true` when the representation bitset gained at least one bit.
    #[inline]
    pub fn record_arith(&self, lhs: Value, rhs: Value) -> bool {
        let bits = ArithFeedback::classify(lhs) | ArithFeedback::classify(rhs);
        self.arith.fetch_or(bits, Ordering::Relaxed) & bits != bits
    }

    /// Mark an arithmetic site for float widening after its first overflow bail.
    /// Returns `true` exactly once for the cell.
    pub fn widen_arith_to_float(&self) -> bool {
        self.arith.fetch_or(ARITH_WIDEN_FLOAT, Ordering::Relaxed) & ARITH_WIDEN_FLOAT == 0
    }

    /// Arithmetic bits consumed by a compile snapshot.
    #[must_use]
    pub fn arith_bits(&self) -> u8 {
        let bits = self.arith.load(Ordering::Relaxed);
        if bits & ARITH_WIDEN_FLOAT != 0 {
            ARITH_INT32 | ARITH_FLOAT64
        } else {
            bits & !ARITH_WIDEN_FLOAT
        }
    }

    /// Record the receiver family observed at one `LoadElement` instruction.
    /// `None` preserves an unseen cell for an ordinary non-typed receiver;
    /// mixed or unsupported typed-array kinds become permanently generic.
    /// Returns `true` when the bounded element kind changes.
    pub fn record_element_load(&self, observed: Option<JitElementLoadKind>) -> bool {
        let Some(observed) = observed else {
            let mut states = self.states.load(Ordering::Relaxed);
            loop {
                let current = states & ELEMENT_MASK;
                if matches!(current, ELEMENT_UNSEEN | ELEMENT_GENERIC) {
                    return false;
                }
                let next = (states & !ELEMENT_MASK) | ELEMENT_GENERIC;
                match self.states.compare_exchange_weak(
                    states,
                    next,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(actual) => states = actual,
                }
            }
        };
        let observed = match observed {
            JitElementLoadKind::Any => ELEMENT_GENERIC,
            JitElementLoadKind::Float64 => ELEMENT_FLOAT64,
            JitElementLoadKind::Int32 => ELEMENT_INT32,
        };
        let mut states = self.states.load(Ordering::Relaxed);
        loop {
            let current = states & ELEMENT_MASK;
            let next_element = match current {
                ELEMENT_UNSEEN => observed,
                value if value == observed => value,
                _ => ELEMENT_GENERIC,
            };
            if next_element == current {
                return false;
            }
            let next = (states & !ELEMENT_MASK) | next_element;
            match self.states.compare_exchange_weak(
                states,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(actual) => states = actual,
            }
        }
    }

    /// Element-load specialization consumed by a compile snapshot.
    #[must_use]
    pub fn element_load_kind(&self) -> JitElementLoadKind {
        match self.states.load(Ordering::Relaxed) & ELEMENT_MASK {
            ELEMENT_FLOAT64 => JitElementLoadKind::Float64,
            ELEMENT_INT32 => JitElementLoadKind::Int32,
            _ => JitElementLoadKind::Any,
        }
    }

    /// Record one bytecode callee at an ordinary `Call` instruction.
    pub(crate) fn record_call_target(&self, callee_fid: u32) -> CallTargetTransition {
        match (self.states.load(Ordering::Acquire) & CALL_MASK) >> CALL_SHIFT {
            CALL_UNSEEN => {
                self.call_target.store(callee_fid, Ordering::Relaxed);
                self.states
                    .fetch_update(Ordering::Release, Ordering::Acquire, |states| {
                        Some((states & !CALL_MASK) | (CALL_MONO << CALL_SHIFT))
                    })
                    .ok();
                CallTargetTransition::BecameMonomorphic
            }
            CALL_MONO if self.call_target.load(Ordering::Relaxed) != callee_fid => {
                self.states
                    .fetch_update(Ordering::Release, Ordering::Acquire, |states| {
                        Some((states & !CALL_MASK) | (CALL_POLY << CALL_SHIFT))
                    })
                    .ok();
                CallTargetTransition::BecamePolymorphic
            }
            _ => CallTargetTransition::Unchanged,
        }
    }

    /// Monomorphic/polymorphic call target observed at this instruction.
    #[must_use]
    pub(crate) fn call_target(&self) -> Option<CallTargetFeedback> {
        match (self.states.load(Ordering::Acquire) & CALL_MASK) >> CALL_SHIFT {
            CALL_UNSEEN => None,
            CALL_MONO => Some(CallTargetFeedback::Mono(
                self.call_target.load(Ordering::Relaxed),
            )),
            _ => Some(CallTargetFeedback::Poly),
        }
    }
}

/// Dense feedback cells and their single material-transition epoch.
///
/// Keeping versioning beside the cells makes feedback one owned runtime
/// artifact instead of a `CodeBlock` field plus a separately coordinated epoch.
/// Property, call, and arithmetic feedback can migrate behind this boundary
/// without teaching executable code how each slot family publishes changes.
#[derive(Debug)]
pub struct FeedbackVector {
    cells: Box<[InstructionFeedback]>,
    typed_slots: Box<[TypedFeedbackSlot]>,
    epoch: AtomicU32,
}

impl Clone for FeedbackVector {
    fn clone(&self) -> Self {
        Self {
            cells: self.cells.clone(),
            typed_slots: self.typed_slots.clone(),
            epoch: AtomicU32::new(self.epoch()),
        }
    }
}

impl FeedbackVector {
    /// Allocate one zeroed feedback cell per canonical instruction.
    #[must_use]
    pub fn with_instruction_count(instruction_count: usize) -> Self {
        Self {
            cells: (0..instruction_count)
                .map(|_| InstructionFeedback::default())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            typed_slots: (0..instruction_count)
                .map(|_| TypedFeedbackSlot::None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            epoch: AtomicU32::new(0),
        }
    }

    /// Allocate dense cells plus bytecode-kind-selected out-of-line payloads.
    #[must_use]
    pub(crate) fn for_instruction_ops(ops: impl IntoIterator<Item = Op>) -> Self {
        let typed_slots: Box<[_]> = ops
            .into_iter()
            .map(TypedFeedbackSlot::for_op)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            cells: (0..typed_slots.len())
                .map(|_| InstructionFeedback::default())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            typed_slots,
            epoch: AtomicU32::new(0),
        }
    }

    /// Read one canonical instruction cell.
    #[must_use]
    pub(crate) fn cell(&self, index: usize) -> Option<&InstructionFeedback> {
        self.cells.get(index)
    }

    /// Pair one canonical cell with this vector's transition epoch.
    #[must_use]
    pub(crate) fn recorder(&self, index: usize) -> Option<InstructionFeedbackRecorder<'_>> {
        self.cell(index)
            .map(|cell| InstructionFeedbackRecorder::new(self, cell))
    }

    /// Property/cache payload for one schema-compatible instruction.
    #[must_use]
    pub(crate) fn property_slot(
        &self,
        index: usize,
        kind: PropertyIcKind,
    ) -> Option<PropertyFeedbackSlot<'_>> {
        let TypedFeedbackSlot::Property(feedback) = self.typed_slots.get(index)? else {
            return None;
        };
        (feedback.kind == kind).then_some(PropertyFeedbackSlot { feedback })
    }

    /// Whether this instruction owns isolate-local method feedback in the
    /// [`crate::interp::FeedbackDirectory`].
    #[must_use]
    pub(crate) fn is_method_slot(&self, index: usize) -> bool {
        matches!(self.typed_slots.get(index), Some(TypedFeedbackSlot::Method))
    }

    /// Ordinary-call payload for one `Call` instruction.
    #[must_use]
    pub(crate) fn call_slot(&self, index: usize) -> Option<CallFeedbackSlot<'_>> {
        let TypedFeedbackSlot::Call(feedback) = self.typed_slots.get(index)? else {
            return None;
        };
        Some(CallFeedbackSlot { feedback })
    }

    /// Record compact and bounded ordinary-call state through one intent-level
    /// operation. Call sites never coordinate the dense cell, payload, or epoch.
    pub(crate) fn record_call(&self, index: usize, callee_fid: u32) -> CallTargetTransition {
        let Some(cell) = self.cell(index) else {
            return CallTargetTransition::Unchanged;
        };
        let transition = cell.record_call_target(callee_fid);
        if transition.state_changed() {
            self.bump_epoch();
        }
        if self
            .call_slot(index)
            .is_some_and(|slot| slot.record(callee_fid) == DistributionTransition::Extended)
        {
            self.bump_epoch();
        }
        transition
    }

    /// Current monotonic version of material feedback transitions.
    #[must_use]
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Advance the feedback epoch without permitting wraparound.
    #[inline]
    pub(crate) fn bump_epoch(&self) {
        let _ = self
            .epoch
            .fetch_update(Ordering::Release, Ordering::Relaxed, |epoch| {
                epoch.checked_add(1)
            });
    }
}

/// One instruction cell paired with its owning feedback-vector epoch.
///
/// Recording through this view preserves the compact eight-byte cell while
/// making the rare transition path advance the single function-wide epoch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InstructionFeedbackRecorder<'a> {
    vector: &'a FeedbackVector,
    cell: &'a InstructionFeedback,
}

impl<'a> InstructionFeedbackRecorder<'a> {
    const fn new(vector: &'a FeedbackVector, cell: &'a InstructionFeedback) -> Self {
        Self { vector, cell }
    }

    #[inline]
    fn note_transition(self, changed: bool) -> bool {
        if changed {
            self.vector.bump_epoch();
        }
        changed
    }

    /// Record arithmetic representations and advance the epoch on new bits.
    #[inline]
    pub(crate) fn record_arith(self, lhs: Value, rhs: Value) -> bool {
        self.note_transition(self.cell.record_arith(lhs, rhs))
    }

    /// Record an element kind and advance the epoch on widening.
    pub(crate) fn record_element_load(self, observed: Option<JitElementLoadKind>) -> bool {
        self.note_transition(self.cell.record_element_load(observed))
    }

    /// Record a branch sample and advance the epoch on a newly seen direction.
    pub(crate) fn record_branch(self, taken: bool) -> bool {
        self.note_transition(self.cell.record_branch(taken))
    }

    /// Mark arithmetic widening and advance the epoch only on its first bail.
    pub(crate) fn widen_arith_to_float(self) -> bool {
        self.note_transition(self.cell.widen_arith_to_float())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_neither_numeric_nor_int32() {
        let fb = ArithFeedback::default();
        assert!(fb.is_empty());
        assert!(!fb.is_numeric_only());
        assert!(!fb.is_int32_only());
    }

    #[test]
    fn pure_int32_site_is_int32_and_numeric() {
        let mut fb = ArithFeedback::default();
        fb.record(Value::number_i32(3), Value::number_i32(4));
        fb.record(Value::number_i32(-1), Value::number_i32(0));
        assert!(fb.is_int32_only());
        assert!(fb.is_numeric_only());
    }

    #[test]
    fn mixed_int_and_double_is_numeric_not_int32() {
        let mut fb = ArithFeedback::default();
        fb.record(Value::number_i32(3), Value::number_f64(2.5));
        assert!(!fb.is_int32_only());
        assert!(fb.is_numeric_only());
    }

    #[test]
    fn any_string_operand_poisons_numeric() {
        let mut fb = ArithFeedback::default();
        fb.record(Value::number_i32(3), Value::number_i32(4));
        fb.record(Value::number_f64(1.0), Value::undefined());
        assert!(!fb.is_numeric_only());
        assert!(!fb.is_int32_only());
        assert_eq!(fb.bits() & ARITH_OTHER, ARITH_OTHER);
    }

    #[test]
    fn dense_cell_widens_once_and_keeps_element_demotion_sticky() {
        let cell = InstructionFeedback::default();
        cell.record_arith(Value::number_i32(1), Value::number_i32(2));
        assert_eq!(cell.arith_bits(), ARITH_INT32);
        assert!(cell.widen_arith_to_float());
        assert!(!cell.widen_arith_to_float());
        assert_eq!(cell.arith_bits(), ARITH_INT32 | ARITH_FLOAT64);

        cell.record_element_load(Some(JitElementLoadKind::Float64));
        assert_eq!(cell.element_load_kind(), JitElementLoadKind::Float64);
        cell.record_element_load(Some(JitElementLoadKind::Int32));
        assert_eq!(cell.element_load_kind(), JitElementLoadKind::Any);
        cell.record_element_load(Some(JitElementLoadKind::Float64));
        assert_eq!(cell.element_load_kind(), JitElementLoadKind::Any);

        let ordinary_then_typed = InstructionFeedback::default();
        ordinary_then_typed.record_element_load(None);
        ordinary_then_typed.record_element_load(Some(JitElementLoadKind::Int32));
        assert_eq!(
            ordinary_then_typed.element_load_kind(),
            JitElementLoadKind::Int32
        );
        ordinary_then_typed.record_element_load(None);
        assert_eq!(
            ordinary_then_typed.element_load_kind(),
            JitElementLoadKind::Any
        );
    }

    #[test]
    fn dense_cell_layout_stays_compact() {
        assert_eq!(std::mem::size_of::<InstructionFeedback>(), 8);
    }

    #[test]
    fn branch_feedback_counts_taken_and_total_compactly() {
        let cell = InstructionFeedback::default();
        assert!(cell.record_branch(true));
        assert!(cell.record_branch(false));
        assert!(!cell.record_branch(true));
        assert_eq!(cell.branch_counts(), (2, 3));
        assert_eq!(cell.clone().branch_counts(), (2, 3));
    }

    #[test]
    fn call_target_tracks_mono_then_poly_without_truncating_ids() {
        let max_id = InstructionFeedback::default();
        assert_eq!(
            max_id.record_call_target(u32::MAX),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(
            max_id.call_target(),
            Some(CallTargetFeedback::Mono(u32::MAX))
        );

        let cell = InstructionFeedback::default();
        assert_eq!(
            cell.record_call_target(7),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(cell.record_call_target(7), CallTargetTransition::Unchanged);
        assert_eq!(cell.call_target(), Some(CallTargetFeedback::Mono(7)));
        assert_eq!(
            cell.record_call_target(9),
            CallTargetTransition::BecamePolymorphic
        );
        assert_eq!(cell.call_target(), Some(CallTargetFeedback::Poly));
        assert_eq!(cell.record_call_target(7), CallTargetTransition::Unchanged);
        assert_eq!(cell.call_target(), Some(CallTargetFeedback::Poly));
    }

    #[test]
    fn vector_epoch_advances_once_per_material_transition() {
        let vector = FeedbackVector::with_instruction_count(1);
        let feedback = vector.recorder(0).unwrap();
        assert_eq!(vector.epoch(), 0);

        assert!(feedback.record_arith(Value::number_i32(1), Value::number_i32(2)));
        assert_eq!(vector.epoch(), 1);
        assert!(!feedback.record_arith(Value::number_i32(3), Value::number_i32(4)));
        assert_eq!(vector.epoch(), 1);
        assert!(feedback.record_arith(Value::number_f64(1.5), Value::number_i32(4)));
        assert_eq!(vector.epoch(), 2);

        assert!(feedback.record_element_load(Some(JitElementLoadKind::Float64)));
        assert_eq!(vector.epoch(), 3);
        assert!(!feedback.record_element_load(Some(JitElementLoadKind::Float64)));
        assert_eq!(vector.epoch(), 3);
        assert!(feedback.record_element_load(Some(JitElementLoadKind::Int32)));
        assert_eq!(vector.epoch(), 4);
        assert!(!feedback.record_element_load(None));
        assert_eq!(vector.epoch(), 4);

        assert!(feedback.record_branch(true));
        assert_eq!(vector.epoch(), 5);
        assert!(!feedback.record_branch(true));
        assert_eq!(vector.epoch(), 5);
        assert!(feedback.record_branch(false));
        assert_eq!(vector.epoch(), 6);

        assert_eq!(
            vector.record_call(0, 7),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(vector.epoch(), 7);
        assert_eq!(vector.record_call(0, 7), CallTargetTransition::Unchanged);
        assert_eq!(vector.epoch(), 7);
        assert_eq!(
            vector.record_call(0, 8),
            CallTargetTransition::BecamePolymorphic
        );
        assert_eq!(vector.epoch(), 8);

        assert!(feedback.widen_arith_to_float());
        assert_eq!(vector.epoch(), 9);
        assert!(!feedback.widen_arith_to_float());
        assert_eq!(vector.epoch(), 9);
        assert_eq!(std::mem::size_of::<InstructionFeedback>(), 8);
    }

    #[test]
    fn cloning_vector_snapshots_cells_and_epoch_without_sharing_mutation() {
        let vector = FeedbackVector::with_instruction_count(2);
        vector
            .recorder(1)
            .unwrap()
            .record_arith(Value::number_i32(1), Value::number_i32(2));

        let cloned = vector.clone();
        assert_eq!(cloned.epoch(), 1);
        assert_eq!(cloned.cell(1).unwrap().arith_bits(), ARITH_INT32);

        cloned.recorder(0).unwrap().record_branch(true);
        assert_eq!(cloned.epoch(), 2);
        assert_eq!(vector.epoch(), 1);
        assert_eq!(vector.cell(0).unwrap().branch_counts(), (0, 0));
    }

    #[test]
    fn typed_atomic_slots_keep_code_blocks_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FeedbackVector>();
    }

    #[test]
    fn property_slot_publishes_stable_own_data_summary() {
        use crate::object;
        use crate::property_atom::{AtomId, AtomizedPropertyKey, PropertyAtom};

        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let mut obj = object::alloc_object_old_for_fixture(&mut heap).expect("object");
        object::set(&mut obj, &mut heap, "x", Value::number_i32(1));
        let key = AtomizedPropertyKey::new(PropertyAtom::new(AtomId::from_constant_index(1)), "x");
        let (stub, _) = CacheStub::install_load(obj, &heap, key).expect("load stub");
        let mut entry = PropertyIcEntry::Empty;
        entry.install(stub);

        let vector = FeedbackVector::for_instruction_ops([Op::LoadProperty]);
        let slot = vector
            .property_slot(0, PropertyIcKind::Load)
            .expect("typed property slot");
        slot.publish(&entry);
        assert!(matches!(
            slot.state(),
            PropertyFeedbackState::MonomorphicOwnData { slot: 0, .. }
        ));
    }

    #[test]
    fn property_seqlock_snapshot_preserves_record_coherence() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let feedback = Arc::new(AtomicPropertyFeedback::new(PropertyIcKind::Load));
        let first = PropertyFeedbackState::MonomorphicOwnData {
            shape_id: crate::object::ShapeId::for_test(11),
            slot: 101,
        };
        let second = PropertyFeedbackState::MonomorphicOwnData {
            shape_id: crate::object::ShapeId::for_test(22),
            slot: 202,
        };
        feedback.publish(first);
        assert_eq!(feedback.snapshot(), first);

        let done = Arc::new(AtomicBool::new(false));
        let writer_feedback = Arc::clone(&feedback);
        let writer_done = Arc::clone(&done);
        let writer = std::thread::spawn(move || {
            for iteration in 0..20_000 {
                writer_feedback.publish(if iteration & 1 == 0 { second } else { first });
            }
            writer_done.store(true, Ordering::Release);
        });

        while !done.load(Ordering::Acquire) {
            assert!(matches!(feedback.snapshot(), state if state == first || state == second));
        }
        writer.join().expect("single feedback writer");
        assert_eq!(feedback.snapshot(), first);
    }

    #[test]
    fn atomic_call_hits_saturate_and_snapshot_without_heap_mutation() {
        let feedback = AtomicCallFeedback::default();
        feedback.targets[0].store(
            pack_call_target(CallTargetCount {
                fid: u32::MAX,
                hits: u32::MAX,
            }),
            Ordering::Relaxed,
        );
        feedback.count.store(1, Ordering::Relaxed);
        feedback
            .state
            .store(CALL_DISTRIBUTION_MONO, Ordering::Relaxed);

        assert_eq!(feedback.record(u32::MAX), DistributionTransition::Unchanged);
        assert_eq!(
            feedback.snapshot(),
            Some(CallSiteDistribution::Mono(CallTargetCount {
                fid: u32::MAX,
                hits: u32::MAX,
            }))
        );
    }
}
