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
//! - Fixed-layout call and executable property-IC slots selected by opcode at
//!   CodeBlock construction; method sites combine a load IC with their method
//!   directory marker.
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
//!   never for an already-recorded observation. Every ordinary-call target
//!   population transition invalidates stale compiled caller plans.
//! - The isolate's VM thread is the sole property-program writer and hot-path
//!   reader. Structural mutation, immutable snapshots, and GC root tracing are
//!   serialized by the slot; probes never borrow interpreter-global state.
//! - Property slots may retain traced transition shapes. No `Value`, upvalue,
//!   closure, or `this` crosses the CodeBlock boundary. Method distributions remain isolate-owned behind
//!   [`crate::interp::MethodFeedbackDirectory`].
//!
//! # See also
//! - [`crate::CodeBlock`] — owner of the live [`FeedbackVector`].

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering, fence};

use otter_bytecode::Op;
use smallvec::SmallVec;

use crate::Value;
use crate::cache_ir::CacheStub;
use crate::property_ic::{PropertyIcEntry, PropertyIcKind};

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
const ELEMENT_DENSE_TAGGED: u8 = 1;
const ELEMENT_TYPED_INT32: u8 = 2;
const ELEMENT_TYPED_FLOAT64: u8 = 3;
const ELEMENT_GENERIC: u8 = 4;
const ELEMENT_DENSE_FLOAT64: u8 = 5;
const ELEMENT_MASK: u8 = 0b0000_0111;

const CALL_ATTEMPTED_SEEN: u8 = 1 << 3;
const BRANCH_TAKEN_SEEN: u8 = 1 << 4;
const BRANCH_NOT_TAKEN_SEEN: u8 = 1 << 5;

/// Material transition made while recording an ordinary call target.
///
/// Baseline invalidation reacts only to [`Self::BecameMonomorphic`]. Keeping
/// that decision distinct from later target-set changes lets the feedback epoch
/// invalidate optimized assumptions without recompiling on every new target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallTargetTransition {
    /// The observation was already represented by the typed call slot.
    Unchanged,
    /// The previously unseen site recorded its first target.
    BecameMonomorphic,
    /// A populated site gained a target or saturated its bounded population.
    BecamePolymorphic,
}

impl CallTargetTransition {
    /// Whether the dense call-target state changed.
    #[must_use]
    pub(crate) const fn state_changed(self) -> bool {
        !matches!(self, Self::Unchanged)
    }

    /// Every new target changes the immutable caller plan. Repeated hits and
    /// observations after saturation do not invalidate an installed generation.
    #[must_use]
    pub(crate) const fn evict_for_reopt(self) -> bool {
        self.state_changed()
    }
}

/// Maximum distinct targets retained at one ordinary-call site.
pub(crate) const MAX_CALL_TARGETS: usize = 8;

/// Stable non-GC identity observed at an ordinary `Op::Call`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrdinaryCallTarget {
    /// Plain bytecode function body.
    Bytecode(u32),
    /// Original bootstrap native reached through a declared leaf entry,
    /// identified by that entry's id.
    StaticNative(crate::native_abi::RuntimeStubId),
}

/// One observed call target and its saturating execution count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallTargetCount {
    pub(crate) target: OrdinaryCallTarget,
    pub(crate) hits: u32,
}

/// Immutable snapshot of the bounded target population for one `Op::Call`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallSiteDistribution {
    Mono(CallTargetCount),
    Poly(Box<SmallVec<[CallTargetCount; MAX_CALL_TARGETS]>>),
    Megamorphic,
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

/// CodeBlock-owned executable property program and its local counters.
///
/// `entry` is read directly only by the isolate VM thread. Mutations, compiler
/// snapshots, and GC tracing take `structural`, so no off-thread reader can
/// observe a `SmallVec` transition. A running store probe deliberately does not
/// hold that lock: it may allocate and synchronously trace this slot's cached
/// transition shape.
#[derive(Debug)]
struct CodeBlockPropertyFeedback {
    kind: PropertyIcKind,
    structural: Mutex<()>,
    entry: UnsafeCell<PropertyIcEntry<CacheStub>>,
    load_hits: AtomicU64,
    load_misses: AtomicU64,
    load_installs: AtomicU64,
    load_disables: AtomicU64,
    store_hits: AtomicU64,
    store_misses: AtomicU64,
    store_installs: AtomicU64,
    store_disables: AtomicU64,
}

// SAFETY: executable property state has one VM-thread writer. Every structural
// mutation and every cross-thread/collector snapshot is serialized by
// `structural`; unlocked entry access is exposed only through the non-Send
// `PropertyFeedbackSlot` view and is read-only while the VM executes a site.
unsafe impl Sync for CodeBlockPropertyFeedback {}
// SAFETY: moving the owning CodeBlock between host threads cannot move or
// access a live isolate's slot concurrently; execution contexts retain the
// owner, while actual probing remains confined to that isolate thread.
unsafe impl Send for CodeBlockPropertyFeedback {}

impl CodeBlockPropertyFeedback {
    fn new(kind: PropertyIcKind) -> Self {
        Self {
            kind,
            structural: Mutex::new(()),
            entry: UnsafeCell::new(PropertyIcEntry::Empty),
            load_hits: AtomicU64::new(0),
            load_misses: AtomicU64::new(0),
            load_installs: AtomicU64::new(0),
            load_disables: AtomicU64::new(0),
            store_hits: AtomicU64::new(0),
            store_misses: AtomicU64::new(0),
            store_installs: AtomicU64::new(0),
            store_disables: AtomicU64::new(0),
        }
    }

    fn entry(&self) -> &PropertyIcEntry<CacheStub> {
        // SAFETY: see the type invariant. This is an isolate-thread read and no
        // structural mutation can run concurrently on that thread.
        unsafe { &*self.entry.get() }
    }

    fn with_entry_mut<R>(&self, f: impl FnOnce(&mut PropertyIcEntry<CacheStub>) -> R) -> R {
        let _guard = self
            .structural
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: `structural` excludes snapshots and the VM-thread invariant
        // excludes a second writer.
        f(unsafe { &mut *self.entry.get() })
    }

    fn snapshot<R>(&self, f: impl FnOnce(&PropertyIcEntry<CacheStub>) -> R) -> R {
        let _guard = self
            .structural
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(self.entry())
    }
}

const CALL_DISTRIBUTION_EMPTY: u8 = 0;
const CALL_DISTRIBUTION_MONO: u8 = 1;
const CALL_DISTRIBUTION_POLY: u8 = 2;
const CALL_DISTRIBUTION_MEGAMORPHIC: u8 = 3;
const CALL_TARGET_BYTECODE: u8 = 0;
const CALL_TARGET_STATIC_NATIVE: u8 = 1;

const fn call_target_kind(target: OrdinaryCallTarget) -> u8 {
    match target {
        OrdinaryCallTarget::Bytecode(_) => CALL_TARGET_BYTECODE,
        OrdinaryCallTarget::StaticNative(_) => CALL_TARGET_STATIC_NATIVE,
    }
}

const fn call_target_payload(target: OrdinaryCallTarget) -> u32 {
    match target {
        OrdinaryCallTarget::Bytecode(fid) => fid,
        OrdinaryCallTarget::StaticNative(stub_id) => stub_id,
    }
}

const fn pack_call_target(target: CallTargetCount) -> u64 {
    (call_target_payload(target.target) as u64) << 32 | target.hits as u64
}

fn unpack_call_target(packed: u64, kind: u8) -> CallTargetCount {
    let payload = (packed >> 32) as u32;
    let target = match kind {
        CALL_TARGET_BYTECODE => OrdinaryCallTarget::Bytecode(payload),
        CALL_TARGET_STATIC_NATIVE => {
            debug_assert!(
                crate::jit_static_native::jit_leaf_builtin(payload).is_some(),
                "native leaf call feedback names a declared entry"
            );
            OrdinaryCallTarget::StaticNative(payload)
        }
        _ => unreachable!("invalid atomic call target kind"),
    };
    CallTargetCount {
        target,
        hits: packed as u32,
    }
}

/// Fixed-capacity ordinary-call distribution. Target records are packed as
/// `(identity payload, hits)` with a parallel one-byte identity class and
/// never allocate after slot construction.
#[derive(Debug)]
struct AtomicCallFeedback {
    sequence: AtomicU32,
    state: AtomicU8,
    count: AtomicU8,
    kinds: [AtomicU8; MAX_CALL_TARGETS],
    targets: [AtomicU64; MAX_CALL_TARGETS],
}

impl Default for AtomicCallFeedback {
    fn default() -> Self {
        Self {
            sequence: AtomicU32::new(0),
            state: AtomicU8::new(CALL_DISTRIBUTION_EMPTY),
            count: AtomicU8::new(0),
            kinds: std::array::from_fn(|_| AtomicU8::new(CALL_TARGET_BYTECODE)),
            targets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl AtomicCallFeedback {
    fn record(&self, observed: OrdinaryCallTarget) -> CallTargetTransition {
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(sequence & 1, 0, "call feedback has one writer");

        let state = self.state.load(Ordering::Relaxed);
        let count = usize::from(self.count.load(Ordering::Relaxed));
        let observed_kind = call_target_kind(observed);
        let mut transition = CallTargetTransition::Unchanged;
        match state {
            CALL_DISTRIBUTION_EMPTY => {
                self.kinds[0].store(observed_kind, Ordering::Relaxed);
                self.targets[0].store(
                    pack_call_target(CallTargetCount {
                        target: observed,
                        hits: 1,
                    }),
                    Ordering::Relaxed,
                );
                self.count.store(1, Ordering::Relaxed);
                self.state.store(CALL_DISTRIBUTION_MONO, Ordering::Relaxed);
                transition = CallTargetTransition::BecameMonomorphic;
            }
            CALL_DISTRIBUTION_MONO | CALL_DISTRIBUTION_POLY => {
                let existing = (0..count).position(|index| {
                    self.kinds[index].load(Ordering::Relaxed) == observed_kind
                        && unpack_call_target(
                            self.targets[index].load(Ordering::Relaxed),
                            observed_kind,
                        )
                        .target
                            == observed
                });
                if let Some(index) = existing {
                    let target = unpack_call_target(
                        self.targets[index].load(Ordering::Relaxed),
                        observed_kind,
                    );
                    self.targets[index].store(
                        pack_call_target(CallTargetCount {
                            hits: target.hits.saturating_add(1),
                            ..target
                        }),
                        Ordering::Relaxed,
                    );
                } else if count < MAX_CALL_TARGETS {
                    self.kinds[count].store(observed_kind, Ordering::Relaxed);
                    self.targets[count].store(
                        pack_call_target(CallTargetCount {
                            target: observed,
                            hits: 1,
                        }),
                        Ordering::Relaxed,
                    );
                    self.count.store((count + 1) as u8, Ordering::Relaxed);
                    self.state.store(CALL_DISTRIBUTION_POLY, Ordering::Relaxed);
                    transition = CallTargetTransition::BecamePolymorphic;
                } else {
                    self.state
                        .store(CALL_DISTRIBUTION_MEGAMORPHIC, Ordering::Relaxed);
                    transition = CallTargetTransition::BecamePolymorphic;
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
            for index in 0..count.min(MAX_CALL_TARGETS) {
                targets.push(unpack_call_target(
                    self.targets[index].load(Ordering::Relaxed),
                    self.kinds[index].load(Ordering::Relaxed),
                ));
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

/// Opcode-selected feedback storage. Boxes are allocated once while the
/// CodeBlock is built. Property payloads own bounded IC programs and may retain
/// traced transition shapes; call payloads remain fixed atomic records.
#[derive(Debug)]
enum TypedFeedbackSlot {
    None,
    Property(Box<CodeBlockPropertyFeedback>),
    Method(Box<CodeBlockPropertyFeedback>),
    Call(Box<AtomicCallFeedback>),
}

impl TypedFeedbackSlot {
    fn for_op(op: Op) -> Self {
        match op {
            Op::LoadProperty => Self::Property(Box::new(CodeBlockPropertyFeedback::new(
                PropertyIcKind::Load,
            ))),
            Op::StoreProperty | Op::StorePropertyStrict => Self::Property(Box::new(
                CodeBlockPropertyFeedback::new(PropertyIcKind::Store),
            )),
            Op::CallMethodValue => Self::Method(Box::new(CodeBlockPropertyFeedback::new(
                PropertyIcKind::Load,
            ))),
            Op::Call
            | Op::CallWithThis
            | Op::CallForwardArguments
            | Op::CallSpread
            | Op::New
            | Op::NewSpread
            | Op::SuperConstruct
            | Op::SuperConstructSpread => Self::Call(Box::default()),
            _ => Self::None,
        }
    }
}

/// Typed view over one `LoadProperty` or `StoreProperty` cache.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PropertyFeedbackSlot<'a> {
    vector: &'a FeedbackVector,
    feedback: &'a CodeBlockPropertyFeedback,
    _vm_thread_only: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl PropertyFeedbackSlot<'_> {
    #[must_use]
    pub(crate) fn snapshot_state(self) -> crate::inspect::IcSiteState {
        self.feedback.snapshot(|entry| match self.feedback.kind {
            PropertyIcKind::Load => crate::inspect::snapshot_load_state(entry),
            PropertyIcKind::Store => crate::inspect::snapshot_store_state(entry),
        })
    }

    #[must_use]
    pub(crate) fn state(self) -> PropertyFeedbackState {
        self.feedback.snapshot(|entry| match entry {
            PropertyIcEntry::Empty => PropertyFeedbackState::Empty,
            PropertyIcEntry::Megamorphic => PropertyFeedbackState::Megamorphic,
            PropertyIcEntry::Polymorphic { entries, .. } => match entries.as_slice() {
                [stub] => {
                    let hit = match self.feedback.kind {
                        PropertyIcKind::Load => stub.own_data_hit(),
                        PropertyIcKind::Store => stub.store_own_data_hit(),
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
        })
    }

    #[must_use]
    pub(crate) fn entry_count(self) -> usize {
        self.feedback.entry().entry_count()
    }

    #[must_use]
    pub(crate) fn is_megamorphic(self) -> bool {
        self.feedback.entry().is_megamorphic()
    }

    pub(crate) fn probe_load(
        self,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
    ) -> Option<Value> {
        self.feedback
            .entry()
            .entries()
            .iter()
            .find_map(|stub| stub.run_load(obj, heap, key))
    }

    pub(crate) fn probe_store(
        self,
        obj: crate::object::JsObject,
        heap: &mut otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
        value: &Value,
    ) -> Result<bool, otter_gc::OutOfMemory> {
        for stub in self.feedback.entry().entries() {
            if stub.run_store(obj, heap, key, value)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn record_hit(self) {
        match self.feedback.kind {
            PropertyIcKind::Load => self.feedback.load_hits.fetch_add(1, Ordering::Relaxed),
            PropertyIcKind::Store => self.feedback.store_hits.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub(crate) fn record_guard_miss(self) -> bool {
        match self.feedback.kind {
            PropertyIcKind::Load => self.feedback.load_misses.fetch_add(1, Ordering::Relaxed),
            PropertyIcKind::Store => self.feedback.store_misses.fetch_add(1, Ordering::Relaxed),
        };
        let became_megamorphic = self
            .feedback
            .with_entry_mut(PropertyIcEntry::record_guard_miss);
        if became_megamorphic {
            match self.feedback.kind {
                PropertyIcKind::Load => self.feedback.load_disables.fetch_add(1, Ordering::Relaxed),
                PropertyIcKind::Store => {
                    self.feedback.store_disables.fetch_add(1, Ordering::Relaxed)
                }
            };
            self.vector.bump_epoch();
        }
        self.is_megamorphic()
    }

    pub(crate) fn record_uncached_miss(self) {
        if self.is_megamorphic() {
            return;
        }
        match self.feedback.kind {
            PropertyIcKind::Load => self.feedback.load_misses.fetch_add(1, Ordering::Relaxed),
            PropertyIcKind::Store => self.feedback.store_misses.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub(crate) fn install(self, stub: CacheStub) {
        if self.is_megamorphic() {
            return;
        }
        let (installed, disabled) = self.feedback.with_entry_mut(|entry| {
            if entry.is_megamorphic() {
                return (false, false);
            }
            let before = entry.entry_count();
            entry.install(stub);
            (
                !entry.is_megamorphic() && entry.entry_count() > before,
                entry.is_megamorphic(),
            )
        });
        if installed {
            match self.feedback.kind {
                PropertyIcKind::Load => self.feedback.load_installs.fetch_add(1, Ordering::Relaxed),
                PropertyIcKind::Store => {
                    self.feedback.store_installs.fetch_add(1, Ordering::Relaxed)
                }
            };
            self.vector.bump_epoch();
        } else if disabled {
            match self.feedback.kind {
                PropertyIcKind::Load => self.feedback.load_disables.fetch_add(1, Ordering::Relaxed),
                PropertyIcKind::Store => {
                    self.feedback.store_disables.fetch_add(1, Ordering::Relaxed)
                }
            };
            self.vector.bump_epoch();
        }
    }

    #[must_use]
    pub(crate) fn mono_load_own_data_hit(self) -> Option<crate::object::AtomOwnPropertyHit> {
        let entries = self.feedback.entry().entries();
        (entries.len() == 1)
            .then(|| entries[0].own_data_hit())
            .flatten()
    }

    pub(crate) fn settled_property_slots(self) -> Option<Vec<(crate::object::ShapeId, u32, u16)>> {
        self.feedback.snapshot(|entry| {
            let stubs = entry.entries();
            let settled: Vec<_> = stubs
                .iter()
                .filter_map(CacheStub::settled_own_slot)
                .collect();
            (!settled.is_empty() && settled.len() == stubs.len()).then_some(settled)
        })
    }

    pub(crate) fn settled_prototype_slots(
        self,
    ) -> Option<Vec<(crate::object::ShapeId, crate::object::ShapeId, u32, u16)>> {
        self.feedback.snapshot(|entry| {
            let stubs = entry.entries();
            let settled: Vec<_> = stubs
                .iter()
                .filter_map(CacheStub::settled_prototype_slot)
                .collect();
            (!settled.is_empty() && settled.len() == stubs.len()).then_some(settled)
        })
    }

    pub(crate) fn whisker_fill(
        self,
        obj: crate::object::JsObject,
        heap: &otter_gc::GcHeap,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
    ) -> Option<crate::jit::JitPropertyIcWay> {
        self.feedback
            .entry()
            .entries()
            .iter()
            .find_map(|stub| stub.lower_jit_way(obj, heap, key))
    }
}

/// Typed view over the bounded target distribution for one ordinary call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CallFeedbackSlot<'a> {
    feedback: &'a AtomicCallFeedback,
}

impl CallFeedbackSlot<'_> {
    fn record(self, target: OrdinaryCallTarget) -> CallTargetTransition {
        self.feedback.record(target)
    }

    #[must_use]
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

    /// Cell a never-executed site inherits from a TypeScript `number`
    /// annotation on both its operands.
    ///
    /// Both numeric bits are set, so the site reads as numeric but not as
    /// `int32`-only: an annotation cannot distinguish the two, and the wider
    /// `Float64` lowering covers every Number. The seed is used only where a
    /// real observation is absent, so a warmed-up site can still narrow to
    /// `int32`.
    #[must_use]
    pub const fn number_annotation_seed() -> Self {
        Self(ARITH_INT32 | ARITH_FLOAT64)
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

    /// The cell after an optimized generation exited at this site.
    ///
    /// Operand observation cannot see an `int32` result that overflowed or a
    /// value the exit refused; the exit itself is the evidence. The site keeps
    /// its numeric observations but is no longer `int32`-only, so a rebuilt
    /// generation speculates no narrower than `Float64` here instead of
    /// re-emitting the speculation that exited.
    #[must_use]
    pub const fn after_optimized_exit(self) -> Self {
        Self(self.0 | ARITH_WIDEN_FLOAT)
    }

    /// `true` when `+` has observed at least one primitive String operand and
    /// no BigInt, object, Symbol, nullish, or Boolean operand. Number bits may
    /// coexist because primitive string concatenation accepts Number on the
    /// other side without observable coercion.
    #[must_use]
    pub const fn is_primitive_string_concat_only(self) -> bool {
        self.0 & ARITH_STRING != 0 && self.0 & (ARITH_BIGINT | ARITH_OTHER) == 0
    }

    /// `true` when this site has no interpreter observation. This does not
    /// prove that the operation is cold: a compiled lower tier may already
    /// execute it without updating this cell. Consumers must either keep the
    /// generic operation or emit guarded speculation with an exact pre-effect
    /// exit.
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
}

impl Clone for InstructionFeedback {
    fn clone(&self) -> Self {
        Self {
            arith: AtomicU8::new(self.arith.load(Ordering::Relaxed)),
            states: AtomicU8::new(self.states.load(Ordering::Acquire)),
            branch_taken: AtomicU8::new(self.branch_taken.load(Ordering::Relaxed)),
            branch_total: AtomicU8::new(self.branch_total.load(Ordering::Relaxed)),
        }
    }
}

impl InstructionFeedback {
    /// Record that one call-family instruction reached semantic dispatch.
    ///
    /// This precedes callable/property resolution, so even a throwing or
    /// otherwise unclassifiable call is no longer mistaken for an unexecuted
    /// cold branch. Returns `true` only on the first attempt.
    pub fn record_call_attempted(&self) -> bool {
        self.states.fetch_or(CALL_ATTEMPTED_SEEN, Ordering::Relaxed) & CALL_ATTEMPTED_SEEN == 0
    }

    /// Whether semantic dispatch has been attempted at this call site.
    #[must_use]
    pub fn call_attempted(&self) -> bool {
        self.states.load(Ordering::Relaxed) & CALL_ATTEMPTED_SEEN != 0
    }

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

    /// Record the receiver family observed at one `LoadElement` instruction.
    /// A site that sees two families becomes permanently generic. Returns
    /// `true` when the bounded family changes.
    pub fn record_element_family(&self, observed: crate::jit::JitElementFamily) -> bool {
        use crate::jit::JitElementFamily as Family;
        let observed = match observed {
            // Empty and holey numeric Arrays are transient construction
            // layouts rather than generated-hit families. Ignore them only
            // while the site is genuinely unseen. Once code has specialized,
            // observing either layout must invalidate that specialization;
            // otherwise its exact kind guard would deopt forever without an
            // epoch change.
            Family::Unseen => None,
            Family::DenseTagged => Some(ELEMENT_DENSE_TAGGED),
            Family::DenseFloat64 => Some(ELEMENT_DENSE_FLOAT64),
            Family::TypedInt32 => Some(ELEMENT_TYPED_INT32),
            Family::TypedFloat64 => Some(ELEMENT_TYPED_FLOAT64),
            Family::Generic => Some(ELEMENT_GENERIC),
        };
        let mut states = self.states.load(Ordering::Relaxed);
        loop {
            let current = states & ELEMENT_MASK;
            let next_family = match (current, observed) {
                (ELEMENT_UNSEEN, None) => ELEMENT_UNSEEN,
                (ELEMENT_UNSEEN, Some(observed)) => observed,
                (_, None) => ELEMENT_GENERIC,
                (value, Some(observed)) if value == observed => value,
                _ => ELEMENT_GENERIC,
            };
            if next_family == current {
                return false;
            }
            let next = (states & !ELEMENT_MASK) | next_family;
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

    /// Element receiver family consumed by a compile snapshot.
    #[must_use]
    pub fn element_family(&self) -> crate::jit::JitElementFamily {
        use crate::jit::JitElementFamily as Family;
        match self.states.load(Ordering::Relaxed) & ELEMENT_MASK {
            ELEMENT_DENSE_TAGGED => Family::DenseTagged,
            ELEMENT_DENSE_FLOAT64 => Family::DenseFloat64,
            ELEMENT_TYPED_INT32 => Family::TypedInt32,
            ELEMENT_TYPED_FLOAT64 => Family::TypedFloat64,
            ELEMENT_GENERIC => Family::Generic,
            _ => Family::Unseen,
        }
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
}

/// Dense feedback cells and their single material-transition epoch.
///
/// Keeping the transition epoch beside the cells makes feedback one owned
/// runtime artifact instead of a `CodeBlock` field plus a separately
/// coordinated counter. Property, call, and arithmetic feedback can evolve
/// behind this boundary without teaching executable code how each slot family
/// publishes changes.
#[derive(Debug)]
pub struct FeedbackVector {
    cells: Box<[InstructionFeedback]>,
    typed_slots: Box<[TypedFeedbackSlot]>,
    epoch: AtomicU32,
}

impl FeedbackVector {
    /// Heap bytes the two dense per-instruction tables retain for the owning
    /// code block's lifetime, including boxed out-of-line slot payloads.
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        let mut total = (std::mem::size_of_val::<[InstructionFeedback]>(&self.cells) as u64)
            .saturating_add(std::mem::size_of_val::<[TypedFeedbackSlot]>(&self.typed_slots) as u64);
        for slot in &self.typed_slots {
            total = total.saturating_add(match slot {
                TypedFeedbackSlot::None => 0,
                TypedFeedbackSlot::Property(payload) | TypedFeedbackSlot::Method(payload) => {
                    std::mem::size_of_val::<CodeBlockPropertyFeedback>(payload) as u64
                }
                TypedFeedbackSlot::Call(payload) => {
                    std::mem::size_of_val::<AtomicCallFeedback>(payload) as u64
                }
            });
        }
        total
    }

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
        let feedback = match self.typed_slots.get(index)? {
            TypedFeedbackSlot::Property(feedback) | TypedFeedbackSlot::Method(feedback) => feedback,
            _ => return None,
        };
        (feedback.kind == kind).then_some(PropertyFeedbackSlot {
            vector: self,
            feedback,
            _vm_thread_only: std::marker::PhantomData,
        })
    }

    /// Whether this instruction owns isolate-local method feedback in the
    /// [`crate::interp::MethodFeedbackDirectory`].
    #[must_use]
    pub(crate) fn is_method_slot(&self, index: usize) -> bool {
        matches!(
            self.typed_slots.get(index),
            Some(TypedFeedbackSlot::Method(_))
        )
    }

    /// Ordinary-call payload for one `Call`, `New`, or `SuperConstruct`
    /// instruction.
    #[must_use]
    pub(crate) fn call_slot(&self, index: usize) -> Option<CallFeedbackSlot<'_>> {
        let TypedFeedbackSlot::Call(feedback) = self.typed_slots.get(index)? else {
            return None;
        };
        Some(CallFeedbackSlot { feedback })
    }

    /// Record bounded ordinary-call state through one intent-level operation.
    /// Call sites never coordinate the payload and epoch independently.
    pub(crate) fn record_call(
        &self,
        index: usize,
        target: OrdinaryCallTarget,
    ) -> CallTargetTransition {
        let Some(slot) = self.call_slot(index) else {
            return CallTargetTransition::Unchanged;
        };
        let transition = slot.record(target);
        if transition.state_changed() {
            self.bump_epoch();
        }
        transition
    }

    /// Current monotonic epoch of material feedback transitions.
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

    pub(crate) fn property_stats(&self) -> crate::property_ic::PropertyIcStats {
        let mut stats = crate::property_ic::PropertyIcStats::default();
        for slot in &self.typed_slots {
            let feedback = match slot {
                TypedFeedbackSlot::Property(feedback) | TypedFeedbackSlot::Method(feedback) => {
                    feedback
                }
                _ => continue,
            };
            stats.load_hits = stats
                .load_hits
                .saturating_add(feedback.load_hits.load(Ordering::Relaxed));
            stats.load_misses = stats
                .load_misses
                .saturating_add(feedback.load_misses.load(Ordering::Relaxed));
            stats.load_installs = stats
                .load_installs
                .saturating_add(feedback.load_installs.load(Ordering::Relaxed));
            stats.load_disables = stats
                .load_disables
                .saturating_add(feedback.load_disables.load(Ordering::Relaxed));
            stats.store_hits = stats
                .store_hits
                .saturating_add(feedback.store_hits.load(Ordering::Relaxed));
            stats.store_misses = stats
                .store_misses
                .saturating_add(feedback.store_misses.load(Ordering::Relaxed));
            stats.store_installs = stats
                .store_installs
                .saturating_add(feedback.store_installs.load(Ordering::Relaxed));
            stats.store_disables = stats
                .store_disables
                .saturating_add(feedback.store_disables.load(Ordering::Relaxed));
        }
        stats
    }

    #[cfg(test)]
    pub(crate) fn polymorphic_property_count(&self, kind: PropertyIcKind) -> usize {
        self.typed_slots
            .iter()
            .filter_map(|slot| match slot {
                TypedFeedbackSlot::Property(feedback) | TypedFeedbackSlot::Method(feedback)
                    if feedback.kind == kind =>
                {
                    Some(feedback.entry())
                }
                _ => None,
            })
            .filter(|entry| entry.is_polymorphic())
            .count()
    }

    pub(crate) fn trace_property_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for slot in &self.typed_slots {
            let feedback = match slot {
                TypedFeedbackSlot::Property(feedback) | TypedFeedbackSlot::Method(feedback) => {
                    feedback
                }
                _ => continue,
            };
            let _guard = feedback
                .structural
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            feedback.entry().trace_roots(visitor);
        }
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

    /// Record an element receiver family and advance the epoch on a change.
    pub(crate) fn record_element_family(self, observed: crate::jit::JitElementFamily) -> bool {
        self.note_transition(self.cell.record_element_family(observed))
    }

    /// Record the first call attempt and advance the owning feedback epoch.
    pub(crate) fn record_call_attempted(self) -> bool {
        self.note_transition(self.cell.record_call_attempted())
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
    fn an_optimized_exit_widens_an_int32_site_but_keeps_it_numeric() {
        let site = ArithFeedback::from_bits(ARITH_INT32);
        assert!(site.is_int32_only());
        let exited = site.after_optimized_exit();
        assert!(!exited.is_int32_only());
        assert!(exited.is_numeric_only());
        assert_eq!(exited.after_optimized_exit(), exited);
    }

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
    fn primitive_string_concat_feedback_excludes_observable_coercion() {
        for bits in [
            ARITH_STRING,
            ARITH_STRING | ARITH_INT32,
            ARITH_STRING | ARITH_FLOAT64,
        ] {
            assert!(ArithFeedback::from_bits(bits).is_primitive_string_concat_only());
        }
        for bits in [
            0,
            ARITH_INT32,
            ARITH_STRING | ARITH_BIGINT,
            ARITH_STRING | ARITH_OTHER,
        ] {
            assert!(!ArithFeedback::from_bits(bits).is_primitive_string_concat_only());
        }
    }

    #[test]
    fn dense_cell_widens_arith_to_float_once() {
        let cell = InstructionFeedback::default();
        cell.record_arith(Value::number_i32(1), Value::number_i32(2));
        assert_eq!(cell.arith_bits(), ARITH_INT32);
        assert!(cell.widen_arith_to_float());
        assert!(!cell.widen_arith_to_float());
        assert_eq!(cell.arith_bits(), ARITH_INT32 | ARITH_FLOAT64);
    }

    #[test]
    fn dense_cell_layout_stays_compact() {
        assert_eq!(std::mem::size_of::<InstructionFeedback>(), 4);
    }

    #[test]
    fn transient_holey_arrays_do_not_poison_packed_numeric_feedback() {
        use crate::jit::JitElementFamily as Family;

        let cell = InstructionFeedback::default();
        assert!(!cell.record_element_family(Family::Unseen));
        assert_eq!(cell.element_family(), Family::Unseen);
        assert!(cell.record_element_family(Family::DenseFloat64));
        assert_eq!(cell.element_family(), Family::DenseFloat64);
        assert!(!cell.record_element_family(Family::DenseFloat64));
        assert!(cell.record_element_family(Family::Unseen));
        assert_eq!(cell.element_family(), Family::Generic);
        assert!(!cell.record_element_family(Family::DenseTagged));
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
    fn call_attempt_feedback_is_monotonic_and_survives_clone() {
        let cell = InstructionFeedback::default();
        assert!(!cell.call_attempted());
        assert!(cell.record_call_attempted());
        assert!(cell.call_attempted());
        assert!(!cell.record_call_attempted());
        assert!(cell.clone().call_attempted());
    }

    #[test]
    fn call_attempt_advances_vector_epoch_once() {
        let vector = FeedbackVector::for_instruction_ops([Op::Call]);
        let feedback = vector.recorder(0).expect("call feedback cell");
        assert!(feedback.record_call_attempted());
        assert_eq!(vector.epoch(), 1);
        assert!(!feedback.record_call_attempted());
        assert_eq!(vector.epoch(), 1);
    }

    #[test]
    fn vector_epoch_advances_once_per_material_transition() {
        let vector = FeedbackVector::for_instruction_ops([Op::Call]);
        let feedback = vector.recorder(0).unwrap();
        assert_eq!(vector.epoch(), 0);

        assert!(feedback.record_arith(Value::number_i32(1), Value::number_i32(2)));
        assert_eq!(vector.epoch(), 1);
        assert!(!feedback.record_arith(Value::number_i32(3), Value::number_i32(4)));
        assert_eq!(vector.epoch(), 1);
        assert!(feedback.record_arith(Value::number_f64(1.5), Value::number_i32(4)));
        assert_eq!(vector.epoch(), 2);

        assert!(feedback.record_branch(true));
        assert_eq!(vector.epoch(), 3);
        assert!(!feedback.record_branch(true));
        assert_eq!(vector.epoch(), 3);
        assert!(feedback.record_branch(false));
        assert_eq!(vector.epoch(), 4);

        assert_eq!(
            vector.record_call(0, OrdinaryCallTarget::Bytecode(7)),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(vector.epoch(), 5);
        assert_eq!(
            vector.record_call(0, OrdinaryCallTarget::Bytecode(7)),
            CallTargetTransition::Unchanged
        );
        assert_eq!(vector.epoch(), 5);
        assert_eq!(
            vector.record_call(0, OrdinaryCallTarget::Bytecode(8)),
            CallTargetTransition::BecamePolymorphic
        );
        assert_eq!(vector.epoch(), 6);

        assert!(feedback.widen_arith_to_float());
        assert_eq!(vector.epoch(), 7);
        assert!(!feedback.widen_arith_to_float());
        assert_eq!(vector.epoch(), 7);
        assert_eq!(std::mem::size_of::<InstructionFeedback>(), 4);
    }

    #[test]
    fn fixed_super_construct_owns_call_feedback() {
        let vector = FeedbackVector::for_instruction_ops([Op::SuperConstruct]);

        assert_eq!(
            vector.record_call(0, OrdinaryCallTarget::Bytecode(17)),
            CallTargetTransition::BecameMonomorphic
        );
        assert_eq!(
            vector.call_slot(0).unwrap().distribution(),
            Some(CallSiteDistribution::Mono(CallTargetCount {
                target: OrdinaryCallTarget::Bytecode(17),
                hits: 1,
            }))
        );
    }

    #[test]
    fn spread_call_family_owns_typed_call_feedback() {
        for op in [Op::CallSpread, Op::NewSpread, Op::SuperConstructSpread] {
            let vector = FeedbackVector::for_instruction_ops([op]);
            assert_eq!(
                vector.record_call(0, OrdinaryCallTarget::Bytecode(23)),
                CallTargetTransition::BecameMonomorphic,
                "{op:?}"
            );
            assert!(matches!(
                vector.call_slot(0).and_then(|slot| slot.distribution()),
                Some(CallSiteDistribution::Mono(CallTargetCount {
                    target: OrdinaryCallTarget::Bytecode(23),
                    hits: 1,
                }))
            ));
        }
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
        let key = AtomizedPropertyKey::new(PropertyAtom::new(AtomId::from_global(1)), "x");
        let resolved = crate::cache_ir::resolve_atom_data_slot(obj, &heap, key).expect("load stub");
        let stub = CacheStub::from_resolved_load(object::shape_id(obj, &heap), &resolved);
        let vector = FeedbackVector::for_instruction_ops([Op::LoadProperty]);
        let slot = vector
            .property_slot(0, PropertyIcKind::Load)
            .expect("typed property slot");
        slot.install(stub);
        assert!(matches!(
            slot.state(),
            PropertyFeedbackState::MonomorphicOwnData { slot: 0, .. }
        ));
    }

    #[test]
    fn has_property_owns_no_typed_property_feedback_slot() {
        let vector = FeedbackVector::for_instruction_ops([Op::HasProperty]);

        assert!(vector.property_slot(0, PropertyIcKind::Load).is_none());
        assert!(vector.property_slot(0, PropertyIcKind::Store).is_none());
    }

    #[test]
    fn method_slot_owns_its_property_program() {
        let vector = FeedbackVector::for_instruction_ops([Op::CallMethodValue]);
        let slot = vector
            .property_slot(0, PropertyIcKind::Load)
            .expect("method load IC");
        assert_eq!(slot.state(), PropertyFeedbackState::Empty);
        assert!(vector.is_method_slot(0));
    }

    #[test]
    fn property_slot_owns_state_counters_and_epoch_until_codeblock_drop() {
        let vector = FeedbackVector::for_instruction_ops([Op::LoadProperty]);
        let slot = vector
            .property_slot(0, PropertyIcKind::Load)
            .expect("load property IC");
        for expected_epoch in 1..=4 {
            slot.install(CacheStub::default());
            assert_eq!(vector.epoch(), expected_epoch);
        }
        for _ in 0..3 {
            assert!(!slot.record_guard_miss());
            assert_eq!(vector.epoch(), 4);
        }
        assert!(slot.record_guard_miss());
        assert_eq!(vector.epoch(), 5);
        for _ in 0..2048 {
            slot.record_uncached_miss();
        }
        assert!(slot.is_megamorphic(), "megamorphic is CodeBlock-terminal");
        assert_eq!(vector.epoch(), 5);

        let stats = vector.property_stats();
        assert_eq!(stats.load_installs, 4);
        assert_eq!(stats.load_misses, 4);
        assert_eq!(stats.load_disables, 1);
    }

    #[test]
    fn strict_store_and_method_ops_receive_schema_typed_property_slots() {
        let vector =
            FeedbackVector::for_instruction_ops([Op::StorePropertyStrict, Op::CallMethodValue]);
        assert!(vector.property_slot(0, PropertyIcKind::Store).is_some());
        assert!(vector.property_slot(0, PropertyIcKind::Load).is_none());
        assert!(vector.property_slot(1, PropertyIcKind::Load).is_some());
        assert!(vector.is_method_slot(1));
    }

    #[test]
    fn atomic_call_hits_saturate_and_snapshot_without_heap_mutation() {
        let feedback = AtomicCallFeedback::default();
        feedback.kinds[0].store(CALL_TARGET_BYTECODE, Ordering::Relaxed);
        feedback.targets[0].store(
            pack_call_target(CallTargetCount {
                target: OrdinaryCallTarget::Bytecode(u32::MAX),
                hits: u32::MAX,
            }),
            Ordering::Relaxed,
        );
        feedback.count.store(1, Ordering::Relaxed);
        feedback
            .state
            .store(CALL_DISTRIBUTION_MONO, Ordering::Relaxed);

        assert_eq!(
            feedback.record(OrdinaryCallTarget::Bytecode(u32::MAX)),
            CallTargetTransition::Unchanged
        );
        assert_eq!(
            feedback.snapshot(),
            Some(CallSiteDistribution::Mono(CallTargetCount {
                target: OrdinaryCallTarget::Bytecode(u32::MAX),
                hits: u32::MAX,
            }))
        );
    }
}
