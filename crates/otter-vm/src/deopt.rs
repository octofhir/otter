//! Exact-PC deopt frame-state and safepoint stack-map ABI.
//!
//! This module defines the logical frame states, typed physical exits, and
//! concrete stack maps that let a moving collector and an optimizing tier
//! coexist. The optimizing tier populates them when it compiles a function;
//! this module fixes their VM-owned shape and reconstitution rules.
//!
//! # Contents
//! - [`FrameState`], [`DeoptSlot`], and [`DeoptTable`] — exact-PC frame
//!   reconstruction metadata.
//! - [`DeoptVerifyLimits`] and [`DeoptVerifyError`] — pure schema verification.
//! - [`StackMap`], [`Safepoint`], and [`SafepointTable`] — compiled-frame GC
//!   root metadata.
//!
//! 1. **Frame-state table** ([`DeoptTable`]) — indexed by logical
//!    [`FrameStateId`]. For each reconstructable state it records an outermost-first chain of
//!    interpreter frames, each at its exact byte-PC, and for every virtual
//!    register says where the value lives ([`DeoptLocation`]) and how to turn
//!    its raw bits back into a full tagged [`Value`] ([`DeoptRepr`]).
//!
//! 2. **Safepoint stack maps** ([`SafepointTable`]) — one [`StackMap`] per
//!    GC-safe point (every call and allocation site), marking which compiled
//!    slots hold a tagged, rootable pointer. The moving collector consults the
//!    map for the active safepoint to find and relocate the roots an optimized
//!    frame holds, without conservatively scanning the stack.
//!
//! # Reconstitution
//!
//! A register held unboxed in compiled code must be re-tagged on the way out.
//! [`DeoptRepr::reconstitute`] is the single source of truth: integer slots
//! re-tag through the matching signed or unsigned Number conversion, Boolean
//! slots re-tag as `false` or `true`, a `Float64` slot re-boxes through
//! [`Value::number_f64`], and a `Tagged` slot is already a full `Value`.
//!
//! # Invariants
//!
//! - A [`DeoptTable`] retains the dense logical [`FrameStateId`] namespace;
//!   entries used only for GC may have no reconstruction recipe. Physical
//!   [`DeoptExitDescriptor`] records separately name that state plus a typed
//!   reason and action. An absent state or safepoint returns `None`.
//! - A [`FrameState`] carries one [`DeoptSlot`] per interpreter virtual
//!   register the frame defines, in register-index order, matching the windowed
//!   register numbering the frame ABI fixes. Its frames are ordered outermost
//!   first and retain their own function identity and exact resume PC.
//! - The same frame and entry schema carries SSA inputs before allocation and
//!   concrete recipes afterwards and decoded values at runtime. Every nested entry
//!   preserves this, closure and new.target with register-slot location bounds.
//! - Literal recipes are not physical locations and may be shared by any
//!   number of slots. They let optimized code omit values needed only by deopt.
//! - A [`StackMap`] indexes the same compiled slots the frame state locates;
//!   bit `i` set means slot `i` holds a tagged pointer the collector relocates.

use crate::Value;
use crate::native_abi::{ExitAction, ExitReason, FrameStateId};

/// Declared bounds used to verify one compiled function's deopt metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeoptVerifyLimits {
    /// Maximum number of interpreter-register slots in one frame state.
    pub max_frame_slots: usize,
    /// Number of machine registers addressable by [`DeoptLocation::Register`].
    pub machine_register_count: u16,
    /// Smallest valid frame-pointer-relative stack-slot byte offset.
    pub min_stack_slot_offset: i32,
    /// Largest valid frame-pointer-relative stack-slot byte offset.
    pub max_stack_slot_offset: i32,
}

/// Failure to verify concrete deopt metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeoptVerifyError {
    /// Stack-slot limits describe an empty or backwards range.
    InvalidStackSlotRange {
        /// Declared minimum byte offset.
        min: i32,
        /// Declared maximum byte offset.
        max: i32,
    },
    /// A frame state contains more interpreter slots than declared.
    FrameSlotCountOutOfRange {
        /// Frame state's exact byte-PC.
        byte_pc: u32,
        /// Declared maximum slot count.
        max: usize,
        /// Stored slot count.
        actual: usize,
    },
    /// A machine-register location exceeds the declared register file.
    MachineRegisterOutOfRange {
        /// Frame state's exact byte-PC.
        byte_pc: u32,
        /// Interpreter slot containing the location.
        slot: usize,
        /// Invalid machine-register id.
        register: u16,
        /// Declared machine-register count.
        register_count: u16,
    },
    /// A stack-slot byte offset exceeds the declared frame range.
    StackSlotOutOfRange {
        /// Frame state's exact byte-PC.
        byte_pc: u32,
        /// Interpreter slot containing the location.
        slot: usize,
        /// Invalid frame-pointer-relative byte offset.
        offset: i32,
        /// Declared minimum byte offset.
        min: i32,
        /// Declared maximum byte offset.
        max: i32,
    },
    /// A stack-slot byte offset is not aligned for its raw 64-bit payload.
    StackSlotMisaligned {
        /// Frame state's exact byte-PC.
        byte_pc: u32,
        /// Interpreter slot containing the location.
        slot: usize,
        /// Misaligned frame-pointer-relative byte offset.
        offset: i32,
    },
    /// An exit rebuilds no frames at all.
    EmptyFrameChain,
    /// Only the outermost frame may omit its call-entry bindings.
    InvalidFrameEntry {
        /// Index in the outermost-first chain.
        frame_index: usize,
    },
    /// A nested result destination lies outside its caller's register window.
    InvalidReturnRegister {
        /// Index of the returning callee in the chain.
        frame_index: usize,
        /// Destination outside the caller register window.
        register: u16,
    },
}

impl std::fmt::Display for DeoptVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid concrete deopt metadata: {self:?}")
    }
}

impl std::error::Error for DeoptVerifyError {}

/// How a deopt slot's raw bits reconstitute into a full tagged [`Value`].
///
/// The optimizing tier may keep a value unboxed across a region (an int in a
/// general register, a double in an FP register); the deopt record names the
/// representation so the exit re-tags it into the boxed `Value` the
/// interpreter frame expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptRepr {
    /// Already a full 8-byte tagged `Value`; the raw bits are the value.
    Tagged,
    /// An unboxed `i32` in the low 32 bits; re-tag to a number `Value`.
    Int32,
    /// An unboxed canonical Boolean encoded as integer zero or one.
    Boolean,
    /// An unboxed `u32` in the low 32 bits; re-tag to a number `Value`.
    Uint32,
    /// An unboxed `f64` bit pattern; re-box to a number `Value`.
    Float64,
}

impl DeoptRepr {
    /// Reconstitute the full tagged [`Value`] from a slot's raw 64-bit payload.
    /// `raw` is the machine word read from the slot's [`DeoptLocation`].
    #[must_use]
    pub fn reconstitute(self, raw: u64) -> Value {
        match self {
            DeoptRepr::Tagged => Value::from_bits(raw),
            DeoptRepr::Int32 => Value::number_i32(raw as u32 as i32),
            DeoptRepr::Boolean => Value::boolean(raw != 0),
            DeoptRepr::Uint32 => Value::number_f64(f64::from(raw as u32)),
            DeoptRepr::Float64 => Value::number_f64(f64::from_bits(raw)),
        }
    }
}

/// Where a value lives at a deopt point, relative to the optimized frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptLocation {
    /// A machine register, by the optimizing tier's register id.
    Register(u16),
    /// A spill stack slot, by signed byte offset from the frame pointer.
    StackSlot(i32),
    /// A compile-time raw literal rematerialized only when this exit runs.
    /// [`DeoptSlot::repr`] defines how the bits become a tagged VM value.
    Literal(u64),
}

/// One interpreter virtual register at a deopt point: where it lives and how to
/// turn it back into a tagged [`Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeoptSlot {
    /// Where the value lives in the optimized frame.
    pub location: DeoptLocation,
    /// How to reconstitute the boxed `Value` from the raw bits at `location`.
    pub repr: DeoptRepr,
}

/// How a spliced callee frame was entered, everything its rebuilt activation
/// needs that its register window does not carry.
///
/// A rebuilt chain is *constructed*, not replayed: the exit hands the
/// interpreter a complete set of frames rather than re-running the caller's
/// call instruction, so the binding a call would have established has to be
/// described here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeoptFrameEntry<Slot = DeoptSlot> {
    /// Caller register the frame's return value is written to.
    pub return_register: u16,
    /// Where the frame's `this` binding lives at this exit.
    pub this: Slot,
    /// Exact callable whose captured cells and self binding the activation owns.
    pub closure: Slot,
    /// The own or lexical new.target binding; undefined when not bound.
    pub new_target: Slot,
}

/// One interpreter frame to rebuild at a deopt point.
///
/// Rebuilding it means materializing each [`DeoptSlot`] (read the raw bits at
/// its location, [`DeoptRepr::reconstitute`]) into the interpreter register of
/// the same index, and resuming that frame at `byte_pc`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeoptFrame<Slot = DeoptSlot> {
    /// VM function id whose body this frame runs.
    pub function_id: u32,
    /// Interpreter byte-PC this frame resumes at.
    pub byte_pc: u32,
    /// How this frame was entered; `None` only for the outermost frame, which
    /// the compiled entry itself owns.
    pub entry: Option<DeoptFrameEntry<Slot>>,
    /// One slot per interpreter virtual register the frame defines, in
    /// register-index order.
    pub slots: Box<[Slot]>,
}

/// The interpreter-state reconstruction record for one deopt point.
///
/// Optimized code may inline callee bodies, so one exit can owe the interpreter
/// a whole chain of frames: the outermost function first, then each inlined
/// callee it was executing, innermost last.
///
/// Only the innermost frame resumes at the instruction that exited. Every
/// caller in the chain had already advanced past its call before its callee's
/// frame was pushed, so a caller's `byte_pc` names the instruction *after* the
/// call, and the register the call writes is left to the ordinary return
/// protocol rather than restored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameState<Slot = DeoptSlot> {
    /// Frames to rebuild, outermost first and innermost last. Never empty.
    pub frames: Box<[DeoptFrame<Slot>]>,
}

impl<Slot> FrameState<Slot> {
    /// The outermost frame — the compiled function's own.
    #[must_use]
    pub fn outermost(&self) -> &DeoptFrame<Slot> {
        self.frames
            .first()
            .expect("a frame state always rebuilds at least its own frame")
    }

    /// The frame optimized code was executing when it exited.
    #[must_use]
    pub fn innermost(&self) -> &DeoptFrame<Slot> {
        self.frames
            .last()
            .expect("a frame state always rebuilds at least its own frame")
    }

    /// `true` when the exit owes the interpreter only the compiled function's
    /// own frame.
    #[must_use]
    pub fn is_single_frame(&self) -> bool {
        self.frames.len() == 1
    }
}

impl FrameState {
    /// Verify slot count and location bounds.
    ///
    /// [`DeoptRepr`] is a closed Rust enum, so every safely constructed value
    /// is intrinsically one of the supported representations.
    pub fn verify(&self, limits: DeoptVerifyLimits) -> Result<(), DeoptVerifyError> {
        if limits.min_stack_slot_offset > limits.max_stack_slot_offset {
            return Err(DeoptVerifyError::InvalidStackSlotRange {
                min: limits.min_stack_slot_offset,
                max: limits.max_stack_slot_offset,
            });
        }
        if self.frames.is_empty() {
            return Err(DeoptVerifyError::EmptyFrameChain);
        }
        for (index, frame) in self.frames.iter().enumerate() {
            if frame.entry.is_some() != (index != 0) {
                return Err(DeoptVerifyError::InvalidFrameEntry { frame_index: index });
            }
            if let Some(entry) = &frame.entry
                && usize::from(entry.return_register) >= self.frames[index - 1].slots.len()
            {
                return Err(DeoptVerifyError::InvalidReturnRegister {
                    frame_index: index,
                    register: entry.return_register,
                });
            }
            frame.verify(limits)?;
        }
        Ok(())
    }
}

impl DeoptFrame {
    /// Verify slot count and location bounds.
    ///
    /// Multiple slots may read the same concrete location. This is required
    /// for ordinary aliases such as a bytecode `LoadLocal`: reconstruction
    /// reads an immutable machine-state snapshot before writing VM registers.
    ///
    /// [`DeoptRepr`] is a closed Rust enum, so every safely constructed value
    /// is intrinsically one of the supported representations.
    pub fn verify(&self, limits: DeoptVerifyLimits) -> Result<(), DeoptVerifyError> {
        if self.slots.len() > limits.max_frame_slots {
            return Err(DeoptVerifyError::FrameSlotCountOutOfRange {
                byte_pc: self.byte_pc,
                max: limits.max_frame_slots,
                actual: self.slots.len(),
            });
        }

        // Activation-only operands obey exactly the same location bounds as
        // register slots. Diagnostic indices append this/closure after the window.
        let entry_slots = self
            .entry
            .iter()
            .flat_map(|entry| [&entry.this, &entry.closure, &entry.new_target]);
        for (slot_index, slot) in self.slots.iter().chain(entry_slots).enumerate() {
            match slot.location {
                DeoptLocation::Register(register) if register >= limits.machine_register_count => {
                    return Err(DeoptVerifyError::MachineRegisterOutOfRange {
                        byte_pc: self.byte_pc,
                        slot: slot_index,
                        register,
                        register_count: limits.machine_register_count,
                    });
                }
                DeoptLocation::StackSlot(offset)
                    if offset < limits.min_stack_slot_offset
                        || offset > limits.max_stack_slot_offset =>
                {
                    return Err(DeoptVerifyError::StackSlotOutOfRange {
                        byte_pc: self.byte_pc,
                        slot: slot_index,
                        offset,
                        min: limits.min_stack_slot_offset,
                        max: limits.max_stack_slot_offset,
                    });
                }
                DeoptLocation::StackSlot(offset)
                    if offset % std::mem::size_of::<u64>() as i32 != 0 =>
                {
                    return Err(DeoptVerifyError::StackSlotMisaligned {
                        byte_pc: self.byte_pc,
                        slot: slot_index,
                        offset,
                    });
                }
                DeoptLocation::Register(_)
                | DeoptLocation::StackSlot(_)
                | DeoptLocation::Literal(_) => {}
            }

            match slot.repr {
                DeoptRepr::Tagged
                | DeoptRepr::Int32
                | DeoptRepr::Boolean
                | DeoptRepr::Uint32
                | DeoptRepr::Float64 => {}
            }
        }
        Ok(())
    }
}

/// Dense identity of one physical exit site in one compiled function.
///
/// Several physical exits may share a logical [`FrameStateId`] while retaining
/// distinct reasons and actions. An interpreter PC cannot identify the site: a
/// body may guard the same instruction more than once, and inlined callee exits
/// can project onto one caller instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeoptExitId(pub u32);

/// Per-compiled-function deopt table, indexed by logical [`FrameStateId`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptTable {
    entries: Vec<Option<FrameState>>,
}

impl DeoptTable {
    /// Build a table from dense logical frame states; each id is its index.
    #[must_use]
    pub fn from_states(states: Vec<FrameState>) -> Self {
        Self {
            entries: states.into_iter().map(Some).collect(),
        }
    }

    /// Build a table in logical frame-state id order. `None` retains an id
    /// used only for GC root derivation and therefore has no deopt recipe.
    #[must_use]
    pub fn from_indexed_states(states: Vec<Option<FrameState>>) -> Self {
        Self { entries: states }
    }

    /// The frame chain for `state`, or `None` when the id has no recipe.
    #[must_use]
    pub fn lookup(&self, state: FrameStateId) -> Option<&FrameState> {
        self.entries.get(state as usize)?.as_ref()
    }

    /// All reconstructable states in logical-id order.
    pub fn entries(&self) -> impl Iterator<Item = &FrameState> {
        self.entries.iter().flatten()
    }

    /// Present concrete states with their stable logical ids.
    pub fn indexed_entries(&self) -> impl Iterator<Item = (FrameStateId, &FrameState)> {
        self.entries.iter().enumerate().filter_map(|(id, state)| {
            state
                .as_ref()
                .map(|state| (u32::try_from(id).unwrap_or(u32::MAX), state))
        })
    }

    /// Number of reconstructable logical states.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    /// Whether the table records no deopt points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    /// Verify every reconstructable state's frame chain and declared bounds.
    pub fn verify(&self, limits: DeoptVerifyLimits) -> Result<(), DeoptVerifyError> {
        if limits.min_stack_slot_offset > limits.max_stack_slot_offset {
            return Err(DeoptVerifyError::InvalidStackSlotRange {
                min: limits.min_stack_slot_offset,
                max: limits.max_stack_slot_offset,
            });
        }
        for state in self.entries.iter().flatten() {
            state.verify(limits)?;
        }
        Ok(())
    }
}

/// Everything one generated exit site needs beyond its [`FrameState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeoptExitDescriptor {
    /// The frame state this site rebuilds, an index into the owning table.
    pub state: FrameStateId,
    /// Typed cause used by exit profiling and diagnostics.
    pub reason: ExitReason,
    /// Cold policy requested by this exact generated site.
    pub action: ExitAction,
    /// Logical (canonical instruction-index) resume PC per frame of the state,
    /// outermost first. The state stores byte PCs, which the interpreter's
    /// frames do not speak; this is the same sequence in their namespace.
    /// Index `0` is the compiled function's own frame.
    pub resume_pcs: Box<[u32]>,
}

/// Deopt metadata a generated exit reads at run time.
///
/// A generated exit site is two instructions: an exit index and a branch to
/// one shared handler, which dumps the machine registers and calls the
/// writeback stub with this record's baked address. Interpreter-state
/// reconstruction is therefore data walked by the stub, never per-exit code.
/// The allocation's address is baked into the generated handler, so it must
/// live exactly as long as the code — the same ownership contract as the
/// property-IC cells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptRuntime {
    /// Frame states indexed by logical [`FrameStateId`].
    pub table: DeoptTable,
    /// Per-exit-site descriptors, indexed by the site index generated code
    /// passes to the writeback stub.
    pub exits: Box<[DeoptExitDescriptor]>,
    /// GPR budget of the allocation the table's register ids refer to; ids at
    /// or past it name FP registers.
    pub gpr_budget: u16,
}

impl DeoptRuntime {
    /// Bytes this deopt metadata retains for the code object's lifetime:
    /// the exit descriptors, every frame state, its frame chain, and each
    /// frame's slot table. Saturating: a saturated total still exceeds any
    /// real budget and fails generated-code admission closed.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        let mut total = std::mem::size_of::<Self>() as u64;
        total = total
            .saturating_add(std::mem::size_of_val::<[DeoptExitDescriptor]>(&self.exits) as u64);
        for state in self.table.entries() {
            total =
                total.saturating_add(std::mem::size_of_val::<[DeoptFrame]>(&state.frames) as u64);
            for frame in &state.frames {
                total =
                    total.saturating_add(std::mem::size_of_val::<[DeoptSlot]>(&frame.slots) as u64);
            }
        }
        total
    }
}

/// A compact bitset over a safepoint's compiled slots: bit `i` set means slot
/// `i` holds a tagged pointer the moving collector must find and relocate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackMap {
    words: Box<[u64]>,
}

impl StackMap {
    /// Build a stack map sized for `slot_count` slots, with the slots in
    /// `tagged` marked. Out-of-range indices are ignored.
    #[must_use]
    pub fn from_tagged_slots(slot_count: usize, tagged: impl IntoIterator<Item = usize>) -> Self {
        let words = slot_count.div_ceil(64);
        let mut bits = vec![0u64; words].into_boxed_slice();
        for slot in tagged {
            if slot < slot_count {
                bits[slot / 64] |= 1u64 << (slot % 64);
            }
        }
        Self { words: bits }
    }

    /// Whether slot `i` holds a tagged root.
    #[must_use]
    pub fn is_tagged(&self, i: usize) -> bool {
        let word = i / 64;
        word < self.words.len() && self.words[word] & (1u64 << (i % 64)) != 0
    }

    /// Visit each tagged slot index in ascending order.
    pub fn for_each_tagged(&self, mut f: impl FnMut(usize)) {
        for (w, &word) in self.words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                f(w * 64 + bit);
                bits &= bits - 1;
            }
        }
    }
}

/// One GC-safe point: the PC it covers and the tagged-slot map at that point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Safepoint {
    /// Interpreter byte-PC of the safe point (a call or allocation site).
    pub byte_pc: u32,
    /// Which compiled slots hold tagged roots at this point.
    pub tagged: StackMap,
}

/// Per-compiled-function safepoint table, looked up by byte-PC.
///
/// Sorted by `byte_pc`; [`Self::lookup`] is an exact-match binary search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafepointTable {
    entries: Vec<Safepoint>,
}

impl SafepointTable {
    /// Build a table from safepoints. They are sorted by `byte_pc`.
    #[must_use]
    pub fn from_safepoints(mut points: Vec<Safepoint>) -> Self {
        points.sort_by_key(|p| p.byte_pc);
        debug_assert!(
            points.windows(2).all(|w| w[0].byte_pc != w[1].byte_pc),
            "two safepoints at the same byte_pc"
        );
        Self { entries: points }
    }

    /// The stack map for `byte_pc`, or `None` when the PC is not a safe point.
    #[must_use]
    pub fn lookup(&self, byte_pc: u32) -> Option<&StackMap> {
        let i = self
            .entries
            .binary_search_by_key(&byte_pc, |p| p.byte_pc)
            .ok()?;
        Some(&self.entries[i].tagged)
    }

    /// Number of recorded safe points.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table records no safe points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify_limits() -> DeoptVerifyLimits {
        DeoptVerifyLimits {
            max_frame_slots: 4,
            machine_register_count: 8,
            min_stack_slot_offset: -64,
            max_stack_slot_offset: 64,
        }
    }

    #[test]
    fn reconstitute_matches_the_value_encoding() {
        assert_eq!(DeoptRepr::Int32.reconstitute(5), Value::number_i32(5));
        assert_eq!(
            DeoptRepr::Int32.reconstitute(u32::MAX as u64),
            Value::number_i32(-1)
        );
        assert_eq!(DeoptRepr::Boolean.reconstitute(0), Value::boolean(false));
        assert_eq!(DeoptRepr::Boolean.reconstitute(1), Value::boolean(true));
        assert_eq!(
            DeoptRepr::Float64.reconstitute(3.5f64.to_bits()),
            Value::number_f64(3.5)
        );
        let value = Value::number_i32(42);
        assert_eq!(DeoptRepr::Tagged.reconstitute(value.to_bits()), value);
    }

    #[test]
    fn reconstitute_round_trips_boundaries_and_special_values() {
        for tagged in [Value::undefined(), Value::null(), Value::number_i32(42)] {
            assert_eq!(DeoptRepr::Tagged.reconstitute(tagged.to_bits()), tagged);
        }

        for integer in [i32::MIN, -1, 0, 1, i32::MAX] {
            assert_eq!(
                DeoptRepr::Int32.reconstitute(integer as u32 as u64),
                Value::number_i32(integer)
            );
        }
        assert_eq!(
            DeoptRepr::Int32.reconstitute(0xdead_beef_8000_0000),
            Value::number_i32(i32::MIN),
            "Int32 uses only the low 32 bits"
        );
        for integer in [0_u32, 1, i32::MAX as u32, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(
                DeoptRepr::Uint32.reconstitute(u64::from(integer)),
                Value::number_f64(f64::from(integer))
            );
        }
        assert_eq!(
            DeoptRepr::Uint32.reconstitute(0xdead_beef_ffff_ffff),
            Value::number_f64(f64::from(u32::MAX)),
            "Uint32 uses only the low 32 bits"
        );

        for number in [0.0, -0.0, 3.5, f64::MIN, f64::MAX, f64::INFINITY] {
            assert_eq!(
                DeoptRepr::Float64.reconstitute(number.to_bits()),
                Value::number_f64(number)
            );
        }
        assert_ne!(
            DeoptRepr::Float64.reconstitute((-0.0_f64).to_bits()),
            DeoptRepr::Float64.reconstitute(0.0_f64.to_bits()),
            "negative zero keeps its sign bit"
        );
        let payload_nan = f64::from_bits(0x7ff8_1234_5678_9abc);
        assert_eq!(
            DeoptRepr::Float64.reconstitute(payload_nan.to_bits()),
            Value::number_f64(f64::NAN),
            "NaN is purified by the frozen Value encoding"
        );
    }

    /// A single-frame state, the shape an exit from a function with nothing
    /// inlined into it produces.
    fn single_frame(byte_pc: u32, slots: Vec<DeoptSlot>) -> FrameState {
        FrameState {
            frames: Box::new([DeoptFrame {
                function_id: 7,
                byte_pc,
                entry: None,
                slots: slots.into(),
            }]),
        }
    }

    #[test]
    fn verifies_inline_entry_operands_and_chain_topology() {
        let slot = DeoptSlot {
            location: DeoptLocation::Literal(0),
            repr: DeoptRepr::Tagged,
        };
        let outer = DeoptFrame {
            function_id: 0,
            byte_pc: 8,
            entry: None,
            slots: Box::new([slot]),
        };
        let inner = DeoptFrame {
            function_id: 1,
            byte_pc: 0,
            entry: Some(DeoptFrameEntry {
                new_target: DeoptSlot {
                    location: DeoptLocation::Literal(Value::undefined().to_bits()),
                    repr: DeoptRepr::Tagged,
                },
                return_register: 0,
                this: slot,
                closure: slot,
            }),
            slots: Box::new([slot]),
        };
        let valid = FrameState {
            frames: Box::new([outer, inner]),
        };
        assert_eq!(valid.verify(verify_limits()), Ok(()));
        for binding in 0..3 {
            let mut invalid = valid.clone();
            let entry = invalid.frames[1].entry.as_mut().unwrap();
            let operand = match binding {
                0 => &mut entry.this,
                1 => &mut entry.closure,
                _ => &mut entry.new_target,
            };
            operand.location = DeoptLocation::Register(u16::MAX);
            assert!(matches!(
                invalid.verify(verify_limits()),
                Err(DeoptVerifyError::MachineRegisterOutOfRange { .. })
            ));
        }
        let mut invalid = valid.clone();
        invalid.frames[1].entry = None;
        assert_eq!(
            invalid.verify(verify_limits()),
            Err(DeoptVerifyError::InvalidFrameEntry { frame_index: 1 })
        );
        let mut invalid = valid.clone();
        invalid.frames[0].entry = valid.frames[1].entry;
        assert_eq!(
            invalid.verify(verify_limits()),
            Err(DeoptVerifyError::InvalidFrameEntry { frame_index: 0 })
        );
        let mut invalid = valid;
        invalid.frames[1].entry.as_mut().unwrap().return_register = 1;
        assert_eq!(
            invalid.verify(verify_limits()),
            Err(DeoptVerifyError::InvalidReturnRegister {
                frame_index: 1,
                register: 1
            })
        );
    }

    #[test]
    fn an_inlined_chain_shares_locations_across_frames() {
        // A callee's parameter is the caller's argument value, so both frames
        // read it from the same immutable machine-state snapshot.
        let shared = DeoptSlot {
            location: DeoptLocation::Register(3),
            repr: DeoptRepr::Tagged,
        };
        let chained = FrameState {
            frames: Box::new([
                DeoptFrame {
                    function_id: 7,
                    byte_pc: 12,
                    entry: None,
                    slots: vec![shared].into(),
                },
                DeoptFrame {
                    function_id: 9,
                    byte_pc: 0,
                    entry: Some(DeoptFrameEntry {
                        new_target: DeoptSlot {
                            location: DeoptLocation::Literal(Value::undefined().to_bits()),
                            repr: DeoptRepr::Tagged,
                        },
                        return_register: 0,
                        this: shared,
                        closure: shared,
                    }),
                    slots: vec![shared].into(),
                },
            ]),
        };

        assert_eq!(chained.verify(verify_limits()), Ok(()));
        assert!(!chained.is_single_frame());
        assert_eq!(chained.outermost().function_id, 7);
        assert_eq!(chained.innermost().function_id, 9);
        // The caller resumes after its call; only the innermost frame resumes
        // at the instruction that exited.
        assert_eq!(chained.outermost().byte_pc, 12);
        assert_eq!(chained.innermost().byte_pc, 0);
    }

    #[test]
    fn a_frame_chain_may_not_be_empty() {
        let empty = FrameState {
            frames: Box::new([]),
        };
        assert_eq!(
            empty.verify(verify_limits()),
            Err(DeoptVerifyError::EmptyFrameChain)
        );
    }

    #[test]
    fn deopt_table_is_keyed_by_exit_id() {
        let slot = DeoptSlot {
            location: DeoptLocation::Register(3),
            repr: DeoptRepr::Int32,
        };
        // Two exits may resume the same PC — a body can guard one instruction
        // more than once — so the id, not the PC, is what names an exit.
        let table = DeoptTable::from_states(vec![
            single_frame(40, vec![slot]),
            single_frame(40, vec![slot]),
        ]);
        table.verify(verify_limits()).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(
            table.lookup(1).unwrap().innermost().slots[0].location,
            slot.location
        );
        assert!(table.lookup(2).is_none());
    }

    #[test]
    fn frame_state_verifier_accepts_well_formed_slots() {
        let state = single_frame(
            12,
            vec![
                DeoptSlot {
                    location: DeoptLocation::Register(3),
                    repr: DeoptRepr::Tagged,
                },
                DeoptSlot {
                    location: DeoptLocation::StackSlot(-8),
                    repr: DeoptRepr::Float64,
                },
                DeoptSlot {
                    location: DeoptLocation::Literal(2),
                    repr: DeoptRepr::Int32,
                },
            ],
        );

        assert_eq!(state.verify(verify_limits()), Ok(()));
    }

    #[test]
    fn frame_state_verifier_accepts_aliases_and_rejects_out_of_range_locations() {
        let aliased = single_frame(
            12,
            vec![
                DeoptSlot {
                    location: DeoptLocation::Register(3),
                    repr: DeoptRepr::Tagged,
                },
                DeoptSlot {
                    location: DeoptLocation::Register(3),
                    repr: DeoptRepr::Int32,
                },
            ],
        );
        assert_eq!(aliased.verify(verify_limits()), Ok(()));

        let repeated_literal = single_frame(
            20,
            vec![
                DeoptSlot {
                    location: DeoptLocation::Literal(4),
                    repr: DeoptRepr::Tagged,
                },
                DeoptSlot {
                    location: DeoptLocation::Literal(4),
                    repr: DeoptRepr::Tagged,
                },
            ],
        );
        assert_eq!(repeated_literal.verify(verify_limits()), Ok(()));
    }

    #[test]
    fn frame_state_verifier_checks_all_declared_bounds() {
        let state_with = |location| {
            single_frame(
                24,
                vec![DeoptSlot {
                    location,
                    repr: DeoptRepr::Tagged,
                }],
            )
        };

        let mut limits = verify_limits();
        limits.max_frame_slots = 0;
        assert!(matches!(
            state_with(DeoptLocation::Register(0)).verify(limits),
            Err(DeoptVerifyError::FrameSlotCountOutOfRange { .. })
        ));
        assert!(matches!(
            state_with(DeoptLocation::Register(8)).verify(verify_limits()),
            Err(DeoptVerifyError::MachineRegisterOutOfRange { .. })
        ));
        assert!(matches!(
            state_with(DeoptLocation::StackSlot(72)).verify(verify_limits()),
            Err(DeoptVerifyError::StackSlotOutOfRange { .. })
        ));
        assert!(matches!(
            state_with(DeoptLocation::StackSlot(4)).verify(verify_limits()),
            Err(DeoptVerifyError::StackSlotMisaligned { .. })
        ));

        let mut invalid_range = verify_limits();
        invalid_range.min_stack_slot_offset = 8;
        invalid_range.max_stack_slot_offset = -8;
        assert_eq!(
            DeoptTable::default().verify(invalid_range),
            Err(DeoptVerifyError::InvalidStackSlotRange { min: 8, max: -8 })
        );
    }

    #[test]
    fn deopt_table_verifies_every_exit() {
        let bad_slot = DeoptSlot {
            location: DeoptLocation::Register(99),
            repr: DeoptRepr::Tagged,
        };
        let table = DeoptTable::from_states(vec![
            single_frame(8, Vec::new()),
            single_frame(20, vec![bad_slot]),
        ]);
        assert!(table.verify(verify_limits()).is_err());
    }

    #[test]
    fn stack_map_marks_only_tagged_slots() {
        let map = StackMap::from_tagged_slots(70, [0usize, 5, 64, 200]);
        assert!(map.is_tagged(0));
        assert!(map.is_tagged(5));
        assert!(map.is_tagged(64));
        assert!(!map.is_tagged(1));
        assert!(!map.is_tagged(69));
        // 200 was out of range and ignored.
        assert!(!map.is_tagged(200));
        let mut seen = Vec::new();
        map.for_each_tagged(|i| seen.push(i));
        assert_eq!(seen, vec![0, 5, 64]);
    }

    #[test]
    fn safepoint_table_lookup() {
        let table = SafepointTable::from_safepoints(vec![
            Safepoint {
                byte_pc: 16,
                tagged: StackMap::from_tagged_slots(4, [1usize]),
            },
            Safepoint {
                byte_pc: 4,
                tagged: StackMap::from_tagged_slots(4, [0usize]),
            },
        ]);
        assert_eq!(table.len(), 2);
        assert!(table.lookup(4).unwrap().is_tagged(0));
        assert!(table.lookup(16).unwrap().is_tagged(1));
        assert!(table.lookup(9).is_none());
    }
}
