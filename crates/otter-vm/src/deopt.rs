//! Exact-PC deopt frame-state and safepoint stack-map ABI.
//!
//! This module defines the logical frame states, typed physical exits, and
//! concrete stack maps that let a moving collector and an optimizing tier
//! coexist. The optimizing tier populates them when it compiles a function;
//! this module fixes their VM-owned shape and reconstitution rules.
//!
//! # Contents
//! - [`FrameState`], [`DeoptSlot`], [`VirtualObject`], and [`DeoptTable`] —
//!   exact-PC frame reconstruction and scalar-replacement metadata.
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
//! - Physical recipes read canonical tagged or untagged homes. Eager/lazy
//!   exit materialization precedes writeback; no register dump crosses this ABI.
//! - Virtual objects are part of the same authoritative frame state. Dense ids
//!   and backward-only object references make materialization order explicit,
//!   acyclic, and independent of target emission.
//! - Retained recipe accounting includes reserved table capacity, absent ids,
//!   resume-PC slices, and every nested owned frame and virtual-field slice.
//! - A [`StackMap`] indexes the same compiled slots the frame state locates;
//!   bit `i` set means slot `i` holds a tagged pointer the collector relocates.

use crate::Value;
use crate::native_abi::{ExitAction, ExitReason, FrameStateId};
use crate::number::NumberValue;

/// Declared bounds used to verify one compiled function's deopt metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeoptVerifyLimits {
    /// Maximum number of interpreter-register slots in one frame state.
    pub max_frame_slots: usize,
    /// Smallest valid compiled-slot-base-relative canonical-home byte offset.
    pub min_stack_slot_offset: i32,
    /// Largest valid compiled-slot-base-relative canonical-home byte offset.
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
    /// A reconstructed register lies outside the frame's window or repeats.
    InvalidFrameRegister {
        /// Frame state's exact byte-PC.
        byte_pc: u32,
        /// Out-of-range or out-of-order register index.
        register: u16,
    },
    /// A nested result destination lies outside its caller's register window.
    InvalidReturnRegister {
        /// Index of the returning callee in the chain.
        frame_index: usize,
        /// Destination outside the caller register window.
        register: u16,
    },
    /// Virtual-object identities are not dense in materialization order.
    NonDenseVirtualObjectId {
        /// Dense identity required at this position.
        expected: u32,
        /// Identity stored by the object recipe.
        actual: u32,
    },
    /// A frame slot names no virtual object in its owning frame state.
    InvalidVirtualObjectReference {
        /// Invalid virtual-object identity.
        object: u32,
        /// Number of recipes in the owning state.
        object_count: usize,
    },
    /// A virtual field refers to itself or a later recipe, creating a cycle or
    /// an undefined materialization dependency.
    InvalidVirtualObjectDependency {
        /// Recipe being verified.
        object: u32,
        /// Invalid dependency identity.
        dependency: u32,
    },
    /// A virtual-object reference carried a non-tagged representation.
    InvalidVirtualObjectRepresentation {
        /// Referenced virtual-object identity.
        object: u32,
    },
    /// A virtual-object recipe cannot represent fields for its allocation kind.
    InvalidVirtualObjectFieldCount {
        /// Recipe identity.
        object: u32,
        /// Unsupported field count.
        fields: usize,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
            // Unboxed numbers return in the canonical Number encoding: an
            // integral value in int32 range other than -0 is an int32, as
            // every other producer boxes it, so operand feedback observed
            // after the exit stays exact.
            DeoptRepr::Uint32 => Value::number(NumberValue::from_f64(f64::from(raw as u32))),
            DeoptRepr::Float64 => Value::number(NumberValue::from_f64(f64::from_bits(raw))),
        }
    }
}

/// Where a value lives at a deopt point, relative to the optimized frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeoptLocation {
    /// A canonical home, by signed byte offset from the compiled slot base.
    StackSlot(i32),
    /// A compile-time raw literal rematerialized only when this exit runs.
    /// [`DeoptSlot::repr`] defines how the bits become a tagged VM value.
    Literal(u64),
    /// A scalar-replaced object materialized from the owning frame state.
    VirtualObject(VirtualObjectId),
}

/// Dense identity of one scalar-replaced object in a [`FrameState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct VirtualObjectId(pub u32);

/// Allocation semantics retained for one scalar-replaced object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VirtualObjectKind {
    /// Ordinary extensible object with `%Object.prototype%` and no own fields.
    PlainObject,
    /// Dense fixed-length ordinary Array. Fields are elements in index order.
    FixedArray,
}

/// One virtual allocation embedded in the authoritative frame state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualObject<Slot> {
    /// Dense materialization identity and order.
    pub id: VirtualObjectId,
    /// Exact canonical allocation semantics.
    pub kind: VirtualObjectKind,
    /// Scalar-replaced fields/elements. Object references must point backward.
    pub fields: Box<[Slot]>,
}

/// Decoded input to the VM-owned virtual-object materializer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualMaterializationValue {
    /// Already reconstructed scalar or tagged field.
    Value(Value),
    /// Reference to an earlier object in the same dense recipe list.
    VirtualObject(VirtualObjectId),
}

/// One interpreter register or virtual field at a deopt point.
///
/// A [`DeoptLocation::VirtualObject`] names a recipe in the same
/// [`FrameState`]; no separate materialization table or emitter-owned state
/// exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptSlot {
    /// Where the value lives or which virtual recipe materializes it.
    pub location: DeoptLocation,
    /// How to reconstitute a physical value from its raw bits. Virtual
    /// references always carry `Tagged`.
    pub repr: DeoptRepr,
}

impl DeoptSlot {
    /// Construct one physical deopt recipe.
    #[must_use]
    pub const fn physical(location: DeoptLocation, repr: DeoptRepr) -> Self {
        Self { location, repr }
    }

    /// Construct one state-local virtual-object reference.
    #[must_use]
    pub const fn virtual_object(object: VirtualObjectId) -> Self {
        Self {
            location: DeoptLocation::VirtualObject(object),
            repr: DeoptRepr::Tagged,
        }
    }

    /// Decode a physical recipe. Virtual objects require state materialization.
    #[must_use]
    pub fn reconstitute(self, raw: impl FnOnce(DeoptLocation) -> u64) -> Option<Value> {
        if matches!(self.location, DeoptLocation::VirtualObject(_)) {
            return None;
        }
        Some(self.repr.reconstitute(raw(self.location)))
    }
}

/// How a spliced callee frame was entered, everything its rebuilt activation
/// needs that its register window does not carry.
///
/// A rebuilt chain is *constructed*, not replayed: the exit hands the
/// interpreter a complete set of frames rather than re-running the caller's
/// call instruction, so the binding a call would have established has to be
/// described here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeoptFrameEntry<Slot = DeoptSlot> {
    /// Caller register the frame's return value is written to.
    pub return_register: u16,
    /// Where the frame's `this` binding lives at this exit.
    pub this: Slot,
    /// Exact callable (SELF) whose context the activation closes over.
    pub closure: Slot,
    /// The own or lexical new.target binding; undefined when not bound.
    pub new_target: Slot,
}

/// One interpreter frame to rebuild at a deopt point.
///
/// Rebuilding it means materializing each [`DeoptSlot`] (read the raw bits at
/// its location, [`DeoptRepr::reconstitute`]) into the interpreter register it
/// names, every other register of the window resuming `undefined`, and
/// resuming that frame at `byte_pc`. A recipe stores only the registers whose
/// value is not the literal `undefined`, the way an optimized-out translation
/// slot costs nothing: a frame's size is its live state, not its window.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeoptFrame<Slot = DeoptSlot> {
    /// VM function id whose body this frame runs.
    pub function_id: u32,
    /// Interpreter byte-PC this frame resumes at.
    pub byte_pc: u32,
    /// How this frame was entered; `None` only for the outermost frame, which
    /// the compiled entry itself owns.
    pub entry: Option<DeoptFrameEntry<Slot>>,
    /// Interpreter virtual registers the rebuilt window defines.
    pub register_count: u16,
    /// The registers this state reconstructs, strictly ascending by index.
    pub slots: Box<[(u16, Slot)]>,
}

impl<Slot> DeoptFrame<Slot> {
    /// A recipe listing every register of its window, in order.
    #[must_use]
    pub fn with_window(
        function_id: u32,
        byte_pc: u32,
        entry: Option<DeoptFrameEntry<Slot>>,
        window: impl IntoIterator<Item = Slot>,
    ) -> Self {
        let slots: Box<[(u16, Slot)]> = window
            .into_iter()
            .enumerate()
            .map(|(register, slot)| {
                (
                    u16::try_from(register).expect("a window fits the register space"),
                    slot,
                )
            })
            .collect();
        Self {
            function_id,
            byte_pc,
            entry,
            register_count: u16::try_from(slots.len()).expect("a window fits the register space"),
            slots,
        }
    }
}

impl<Slot: Clone> DeoptFrame<Slot> {
    /// The complete register window: each reconstructed register's slot,
    /// `fill` everywhere else.
    #[must_use]
    pub fn dense(&self, fill: Slot) -> Vec<Slot> {
        let mut window = vec![fill; usize::from(self.register_count)];
        for (register, slot) in self.slots.iter() {
            window[usize::from(*register)] = slot.clone();
        }
        window
    }
}

/// The interpreter-state reconstruction record for one deopt point.
///
/// Optimized code may inline callee bodies, so one exit can owe the interpreter
/// a whole chain of frames: the outermost function first, then each inlined
/// callee it was executing, innermost last.
///
/// Only the innermost frame resumes at the instruction that exited. A
/// caller's `byte_pc` names the instruction *after* its call; reconstruction
/// stands it on the call instruction until the callee returns, and the
/// register the call writes is left to the ordinary return protocol rather
/// than restored here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FrameState<Slot = DeoptSlot> {
    /// Frames to rebuild, outermost first and innermost last. Never empty.
    pub frames: Box<[DeoptFrame<Slot>]>,
    /// Dense scalar-replaced allocations materialized in id order.
    pub virtual_objects: Box<[VirtualObject<Slot>]>,
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
                && entry.return_register >= self.frames[index - 1].register_count
            {
                return Err(DeoptVerifyError::InvalidReturnRegister {
                    frame_index: index,
                    register: entry.return_register,
                });
            }
            frame.verify_with_virtuals(limits, self.virtual_objects.len())?;
        }
        for (expected, object) in self.virtual_objects.iter().enumerate() {
            let expected = expected as u32;
            if object.id.0 != expected {
                return Err(DeoptVerifyError::NonDenseVirtualObjectId {
                    expected,
                    actual: object.id.0,
                });
            }
            if object.kind == VirtualObjectKind::PlainObject && !object.fields.is_empty() {
                return Err(DeoptVerifyError::InvalidVirtualObjectFieldCount {
                    object: object.id.0,
                    fields: object.fields.len(),
                });
            }
            for field in &object.fields {
                verify_slot(
                    field,
                    limits,
                    self.innermost().byte_pc,
                    usize::MAX,
                    self.virtual_objects.len(),
                )?;
                if let DeoptLocation::VirtualObject(dependency) = field.location
                    && dependency.0 >= object.id.0
                {
                    return Err(DeoptVerifyError::InvalidVirtualObjectDependency {
                        object: object.id.0,
                        dependency: dependency.0,
                    });
                }
            }
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
        self.verify_with_virtuals(limits, 0)
    }

    fn verify_with_virtuals(
        &self,
        limits: DeoptVerifyLimits,
        virtual_object_count: usize,
    ) -> Result<(), DeoptVerifyError> {
        if usize::from(self.register_count) > limits.max_frame_slots {
            return Err(DeoptVerifyError::FrameSlotCountOutOfRange {
                byte_pc: self.byte_pc,
                max: limits.max_frame_slots,
                actual: usize::from(self.register_count),
            });
        }
        let mut next = 0u32;
        for &(register, _) in self.slots.iter() {
            if u32::from(register) < next || register >= self.register_count {
                return Err(DeoptVerifyError::InvalidFrameRegister {
                    byte_pc: self.byte_pc,
                    register,
                });
            }
            next = u32::from(register) + 1;
        }

        // Activation-only operands obey exactly the same location bounds as
        // register slots. Diagnostic indices append this/closure after the window.
        let register_slots = self
            .slots
            .iter()
            .map(|(register, slot)| (usize::from(*register), slot));
        let entry_slots = self
            .entry
            .iter()
            .flat_map(|entry| [&entry.this, &entry.closure, &entry.new_target])
            .enumerate()
            .map(|(index, slot)| (usize::from(self.register_count) + index, slot));
        for (slot_index, slot) in register_slots.chain(entry_slots) {
            verify_slot(slot, limits, self.byte_pc, slot_index, virtual_object_count)?;
        }
        Ok(())
    }
}

fn verify_slot(
    slot: &DeoptSlot,
    limits: DeoptVerifyLimits,
    byte_pc: u32,
    slot_index: usize,
    virtual_object_count: usize,
) -> Result<(), DeoptVerifyError> {
    match slot.location {
        DeoptLocation::VirtualObject(object) => {
            if slot.repr != DeoptRepr::Tagged {
                return Err(DeoptVerifyError::InvalidVirtualObjectRepresentation {
                    object: object.0,
                });
            }
            if object.0 as usize >= virtual_object_count {
                return Err(DeoptVerifyError::InvalidVirtualObjectReference {
                    object: object.0,
                    object_count: virtual_object_count,
                });
            }
        }
        location => {
            match location {
                DeoptLocation::StackSlot(offset)
                    if offset < limits.min_stack_slot_offset
                        || offset > limits.max_stack_slot_offset =>
                {
                    return Err(DeoptVerifyError::StackSlotOutOfRange {
                        byte_pc,
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
                        byte_pc,
                        slot: slot_index,
                        offset,
                    });
                }
                DeoptLocation::StackSlot(_) | DeoptLocation::Literal(_) => {}
                DeoptLocation::VirtualObject(_) => unreachable!(),
            }
            match slot.repr {
                DeoptRepr::Tagged
                | DeoptRepr::Int32
                | DeoptRepr::Boolean
                | DeoptRepr::Uint32
                | DeoptRepr::Float64 => {}
            }
        }
    }
    Ok(())
}

/// Dense identity of one physical exit site in one compiled function.
///
/// Several physical exits may share a logical [`FrameStateId`] while retaining
/// distinct reasons and actions. An interpreter PC cannot identify the site: a
/// body may guard the same instruction more than once, and inlined callee exits
/// can project onto one caller instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeoptExitId(pub u32);

/// One retained frame-state value: location kind and representation in
/// `tag`, the stack offset, literal-pool index or virtual-object id in
/// `payload`. A table keeps eight bytes per reconstructed value, as a V8
/// translation array keeps a compact operand stream; recipes are unpacked
/// only on the cold exit that uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackedDeoptSlot {
    tag: u32,
    payload: u32,
}

impl PackedDeoptSlot {
    const STACK: u32 = 0;
    const LITERAL: u32 = 1;
    const VIRTUAL: u32 = 2;

    fn pack(slot: &DeoptSlot, literals: &mut LiteralPool) -> Self {
        let (kind, payload) = match slot.location {
            DeoptLocation::StackSlot(offset) => (Self::STACK, offset as u32),
            DeoptLocation::Literal(bits) => (Self::LITERAL, literals.intern(bits)),
            DeoptLocation::VirtualObject(VirtualObjectId(id)) => (Self::VIRTUAL, id),
        };
        let repr = match slot.repr {
            DeoptRepr::Tagged => 0,
            DeoptRepr::Int32 => 1,
            DeoptRepr::Boolean => 2,
            DeoptRepr::Uint32 => 3,
            DeoptRepr::Float64 => 4,
        };
        Self {
            tag: kind | (repr << 2),
            payload,
        }
    }

    fn unpack(self, literals: &[u64]) -> DeoptSlot {
        let location = match self.tag & 3 {
            Self::STACK => DeoptLocation::StackSlot(self.payload as i32),
            Self::LITERAL => DeoptLocation::Literal(literals[self.payload as usize]),
            _ => DeoptLocation::VirtualObject(VirtualObjectId(self.payload)),
        };
        let repr = match self.tag >> 2 {
            0 => DeoptRepr::Tagged,
            1 => DeoptRepr::Int32,
            2 => DeoptRepr::Boolean,
            3 => DeoptRepr::Uint32,
            _ => DeoptRepr::Float64,
        };
        DeoptSlot { location, repr }
    }
}

/// Deduplicated literal bits shared by every state of one table.
#[derive(Default)]
struct LiteralPool {
    bits: Vec<u64>,
    index: rustc_hash::FxHashMap<u64, u32>,
}

impl LiteralPool {
    fn intern(&mut self, bits: u64) -> u32 {
        *self.index.entry(bits).or_insert_with(|| {
            self.bits.push(bits);
            u32::try_from(self.bits.len() - 1).expect("deopt literal pool fits u32")
        })
    }
}

impl<A> FrameState<A> {
    /// The same recipe with every slot mapped through `f`.
    fn map_slots<B>(&self, mut f: impl FnMut(&A) -> B) -> FrameState<B> {
        FrameState {
            frames: self
                .frames
                .iter()
                .map(|frame| DeoptFrame {
                    function_id: frame.function_id,
                    byte_pc: frame.byte_pc,
                    entry: frame.entry.as_ref().map(|entry| DeoptFrameEntry {
                        return_register: entry.return_register,
                        this: f(&entry.this),
                        closure: f(&entry.closure),
                        new_target: f(&entry.new_target),
                    }),
                    register_count: frame.register_count,
                    slots: frame
                        .slots
                        .iter()
                        .map(|(register, slot)| (*register, f(slot)))
                        .collect(),
                })
                .collect(),
            virtual_objects: self
                .virtual_objects
                .iter()
                .map(|object| VirtualObject {
                    id: object.id,
                    kind: object.kind,
                    fields: object.fields.iter().map(&mut f).collect(),
                })
                .collect(),
        }
    }
}

/// Per-compiled-function deopt table, indexed by logical [`FrameStateId`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptTable {
    entries: Vec<Option<FrameState<PackedDeoptSlot>>>,
    /// Literal bits every packed literal slot indexes.
    literals: Box<[u64]>,
}

impl DeoptTable {
    /// Build a table from dense logical frame states; each id is its index.
    #[must_use]
    pub fn from_states(states: Vec<FrameState>) -> Self {
        Self::from_indexed_states(states.into_iter().map(Some).collect())
    }

    /// Build a table in logical frame-state id order. `None` retains an id
    /// used only for GC root derivation and therefore has no deopt recipe.
    #[must_use]
    pub fn from_indexed_states(states: Vec<Option<FrameState>>) -> Self {
        let mut literals = LiteralPool::default();
        let entries = states
            .iter()
            .map(|state| {
                state
                    .as_ref()
                    .map(|state| state.map_slots(|slot| PackedDeoptSlot::pack(slot, &mut literals)))
            })
            .collect();
        Self {
            entries,
            literals: literals.bits.into_boxed_slice(),
        }
    }

    /// The frame chain for `state`, unpacked, or `None` when the id has no
    /// recipe.
    #[must_use]
    pub fn lookup(&self, state: FrameStateId) -> Option<FrameState> {
        self.entries
            .get(state as usize)?
            .as_ref()
            .map(|state| self.unpack(state))
    }

    fn unpack(&self, state: &FrameState<PackedDeoptSlot>) -> FrameState {
        state.map_slots(|slot| slot.unpack(&self.literals))
    }

    /// All reconstructable states in logical-id order, unpacked.
    pub fn entries(&self) -> impl Iterator<Item = FrameState> + '_ {
        self.entries
            .iter()
            .flatten()
            .map(|state| self.unpack(state))
    }

    /// Present concrete states with their stable logical ids, unpacked.
    pub fn indexed_entries(&self) -> impl Iterator<Item = (FrameStateId, FrameState)> + '_ {
        self.entries.iter().enumerate().filter_map(|(id, state)| {
            state
                .as_ref()
                .map(|state| (u32::try_from(id).unwrap_or(u32::MAX), self.unpack(state)))
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
        for state in self.entries() {
            state.verify(limits)?;
        }
        Ok(())
    }

    /// Owned packed recipe bytes: reserved id capacity, frames, slot tables,
    /// virtual-object fields and the literal pool.
    fn retained_bytes(&self) -> u64 {
        let mut total = (self.entries.capacity() as u64)
            .saturating_mul(std::mem::size_of::<Option<FrameState<PackedDeoptSlot>>>() as u64)
            .saturating_add(std::mem::size_of_val::<[u64]>(&self.literals) as u64);
        for state in self.entries.iter().flatten() {
            total = total
                .saturating_add(std::mem::size_of_val::<[DeoptFrame<PackedDeoptSlot>]>(
                    &state.frames,
                ) as u64)
                .saturating_add(std::mem::size_of_val::<[VirtualObject<PackedDeoptSlot>]>(
                    &state.virtual_objects,
                ) as u64);
            for frame in &state.frames {
                total = total.saturating_add(std::mem::size_of_val::<[(u16, PackedDeoptSlot)]>(
                    &frame.slots,
                ) as u64);
            }
            for object in &state.virtual_objects {
                total = total.saturating_add(std::mem::size_of_val::<[PackedDeoptSlot]>(
                    &object.fields,
                ) as u64);
            }
        }
        total
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
    /// The code object's safepoint record rooting exactly this recipe's
    /// tagged homes, which writeback publishes before anything may collect;
    /// [`crate::native_abi::NO_SAFEPOINT`] when the recipe reads no home.
    pub safepoint: crate::native_abi::SafepointId,
}

/// Deopt metadata a generated exit reads at run time.
///
/// A generated exit site is two instructions: an exit index and a branch to
/// one shared handler, which calls the writeback stub with this record's baked
/// address after the exit has materialized its canonical homes. Interpreter-state
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
}

impl DeoptRuntime {
    /// Bytes this deopt metadata retains for the code object's lifetime:
    /// the owning box, reserved table capacity (including absent ids), exit
    /// descriptors and resume PCs, every frame chain and slot table, and
    /// virtual-object recipes. This measures requested owned payload backing,
    /// excluding allocator bookkeeping. Saturation fails admission closed.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        let mut total = (std::mem::size_of::<Self>() as u64)
            .saturating_add(self.table.retained_bytes())
            .saturating_add(std::mem::size_of_val::<[DeoptExitDescriptor]>(&self.exits) as u64);
        for exit in &self.exits {
            total = total.saturating_add(std::mem::size_of_val(exit.resume_pcs.as_ref()) as u64);
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
            min_stack_slot_offset: -64,
            max_stack_slot_offset: 64,
        }
    }

    #[test]
    fn retained_bytes_count_reserved_states_resume_pcs_and_nested_recipes() {
        let literal = DeoptSlot::physical(
            DeoptLocation::Literal(Value::undefined().to_bits()),
            DeoptRepr::Tagged,
        );
        let state = FrameState {
            frames: Box::new([
                DeoptFrame::with_window(7, 11, None, [literal; 3]),
                DeoptFrame::with_window(
                    8,
                    12,
                    Some(DeoptFrameEntry {
                        return_register: 0,
                        this: literal,
                        closure: literal,
                        new_target: literal,
                    }),
                    [literal; 2],
                ),
            ]),
            virtual_objects: Box::new([VirtualObject {
                id: VirtualObjectId(0),
                kind: VirtualObjectKind::FixedArray,
                fields: Box::new([literal; 2]),
            }]),
        };
        state.verify(verify_limits()).unwrap();
        let mut entries = Vec::with_capacity(16);
        entries.push(None);
        entries.push(Some(state));
        let mut runtime = DeoptRuntime {
            table: DeoptTable::from_indexed_states(entries),
            exits: Box::new([DeoptExitDescriptor {
                state: 1,
                reason: ExitReason::RuntimeTransition,
                action: ExitAction::Resume,
                resume_pcs: Box::new([1, 2]),
                safepoint: crate::native_abi::NO_SAFEPOINT,
            }]),
        };
        let capacity = runtime.table.entries.capacity();
        // Packed recipes: one deduplicated literal shared by all seven slots.
        let expected = std::mem::size_of::<DeoptRuntime>()
            + capacity * std::mem::size_of::<Option<FrameState<PackedDeoptSlot>>>()
            + std::mem::size_of::<u64>()
            + std::mem::size_of::<DeoptExitDescriptor>()
            + 2 * std::mem::size_of::<u32>()
            + 2 * std::mem::size_of::<DeoptFrame<PackedDeoptSlot>>()
            + 5 * std::mem::size_of::<(u16, PackedDeoptSlot)>()
            + 2 * std::mem::size_of::<PackedDeoptSlot>()
            + std::mem::size_of::<VirtualObject<PackedDeoptSlot>>();
        assert_eq!(runtime.retained_bytes(), expected as u64);
        // An absent id is charged as reserved capacity, not as a recipe.
        runtime.table.entries.push(None);
        let grown = runtime.table.entries.capacity() - capacity;
        let expected =
            expected + grown * std::mem::size_of::<Option<FrameState<PackedDeoptSlot>>>();
        let capacity = runtime.table.entries.capacity();
        assert_eq!(runtime.retained_bytes(), expected as u64);
        runtime.exits[0].resume_pcs = Box::default();
        assert_eq!(
            runtime.retained_bytes(),
            (expected - 2 * std::mem::size_of::<u32>()) as u64,
        );
        let before = runtime.retained_bytes();
        runtime.table.entries.reserve(64);
        let extra_capacity = runtime.table.entries.capacity() - capacity;
        assert!(extra_capacity > 0);
        assert_eq!(
            runtime.retained_bytes() - before,
            (extra_capacity * std::mem::size_of::<Option<FrameState<PackedDeoptSlot>>>()) as u64,
        );
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
        for integer in [0_u32, 1, i32::MAX as u32] {
            assert_eq!(
                DeoptRepr::Uint32.reconstitute(u64::from(integer)),
                Value::number_i32(integer as i32),
                "a Uint32 in int32 range is a canonical int32"
            );
        }
        for integer in [i32::MAX as u32 + 1, u32::MAX] {
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

        for number in [-0.0, 3.5, f64::MIN, f64::MAX, f64::INFINITY] {
            assert_eq!(
                DeoptRepr::Float64.reconstitute(number.to_bits()),
                Value::number_f64(number)
            );
        }
        for integer in [i32::MIN, -1, 0, 1, i32::MAX] {
            assert_eq!(
                DeoptRepr::Float64.reconstitute(f64::from(integer).to_bits()),
                Value::number_i32(integer),
                "an integral Float64 in int32 range is a canonical int32"
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
            frames: Box::new([DeoptFrame::with_window(7, byte_pc, None, slots)]),
            virtual_objects: Box::default(),
        }
    }

    #[test]
    fn verifies_inline_entry_operands_and_chain_topology() {
        let slot = DeoptSlot {
            location: DeoptLocation::Literal(0),
            repr: DeoptRepr::Tagged,
        };
        let outer = DeoptFrame::with_window(0, 8, None, [slot]);
        let inner = DeoptFrame::with_window(
            1,
            0,
            Some(DeoptFrameEntry {
                new_target: DeoptSlot {
                    location: DeoptLocation::Literal(Value::undefined().to_bits()),
                    repr: DeoptRepr::Tagged,
                },
                return_register: 0,
                this: slot,
                closure: slot,
            }),
            [slot],
        );
        let valid = FrameState {
            frames: Box::new([outer, inner]),
            virtual_objects: Box::default(),
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
            operand.location = DeoptLocation::StackSlot(72);
            assert!(matches!(
                invalid.verify(verify_limits()),
                Err(DeoptVerifyError::StackSlotOutOfRange { .. })
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
        // read it from the same canonical home.
        let shared = DeoptSlot {
            location: DeoptLocation::StackSlot(24),
            repr: DeoptRepr::Tagged,
        };
        let chained = FrameState {
            frames: Box::new([
                DeoptFrame::with_window(7, 12, None, vec![shared]),
                DeoptFrame::with_window(
                    9,
                    0,
                    Some(DeoptFrameEntry {
                        new_target: DeoptSlot {
                            location: DeoptLocation::Literal(Value::undefined().to_bits()),
                            repr: DeoptRepr::Tagged,
                        },
                        return_register: 0,
                        this: shared,
                        closure: shared,
                    }),
                    vec![shared],
                ),
            ]),
            virtual_objects: Box::default(),
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
            virtual_objects: Box::default(),
        };
        assert_eq!(
            empty.verify(verify_limits()),
            Err(DeoptVerifyError::EmptyFrameChain)
        );
    }

    #[test]
    fn deopt_table_is_keyed_by_exit_id() {
        let slot = DeoptSlot {
            location: DeoptLocation::StackSlot(24),
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
            table.lookup(1).unwrap().innermost().slots[0].1.location,
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
                    location: DeoptLocation::StackSlot(24),
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
                    location: DeoptLocation::StackSlot(24),
                    repr: DeoptRepr::Tagged,
                },
                DeoptSlot {
                    location: DeoptLocation::StackSlot(24),
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
    fn frame_state_verifies_dense_nested_virtual_objects() {
        let physical = DeoptSlot::physical(
            DeoptLocation::Literal(Value::number_i32(7).to_bits()),
            DeoptRepr::Tagged,
        );
        let mut state = single_frame(20, vec![DeoptSlot::virtual_object(VirtualObjectId(1))]);
        state.virtual_objects = Box::new([
            VirtualObject {
                id: VirtualObjectId(0),
                kind: VirtualObjectKind::FixedArray,
                fields: Box::new([physical]),
            },
            VirtualObject {
                id: VirtualObjectId(1),
                kind: VirtualObjectKind::FixedArray,
                fields: Box::new([DeoptSlot::virtual_object(VirtualObjectId(0))]),
            },
        ]);
        assert_eq!(state.verify(verify_limits()), Ok(()));

        state.virtual_objects[0].fields = Box::new([DeoptSlot::virtual_object(VirtualObjectId(1))]);
        assert_eq!(
            state.verify(verify_limits()),
            Err(DeoptVerifyError::InvalidVirtualObjectDependency {
                object: 0,
                dependency: 1,
            })
        );
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
            state_with(DeoptLocation::StackSlot(0)).verify(limits),
            Err(DeoptVerifyError::FrameSlotCountOutOfRange { .. })
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
            location: DeoptLocation::StackSlot(792),
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
